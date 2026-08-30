use orca_core::thread_identity::{ConversationItemId, TurnId};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeSet;
use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::{Component, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering, compiler_fence};

#[cfg(test)]
thread_local! {
    static DISPLAY_TEXT_APPENDED_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub const SAFE_DIAGNOSTIC_TEXT_BYTE_LIMIT: usize = 4_096;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct NonEmptyText(String);

impl NonEmptyText {
    pub fn try_new(value: impl Into<String>) -> Result<Self, SurfaceValueError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(SurfaceValueError::Empty);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for NonEmptyText {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::try_new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DisplayText(String);

impl DisplayText {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn push_str(&mut self, value: &str) {
        #[cfg(test)]
        DISPLAY_TEXT_APPENDED_BYTES
            .with(|bytes| bytes.set(bytes.get().saturating_add(value.len())));
        self.0.push_str(value);
    }

    #[cfg(test)]
    pub(crate) fn reset_appended_byte_count() {
        DISPLAY_TEXT_APPENDED_BYTES.with(|bytes| bytes.set(0));
    }

    #[cfg(test)]
    pub(crate) fn appended_byte_count() -> usize {
        DISPLAY_TEXT_APPENDED_BYTES.with(std::cell::Cell::get)
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct SafeDiagnosticText(String);

impl SafeDiagnosticText {
    pub fn try_new(value: impl Into<String>) -> Result<Self, SurfaceValueError> {
        let value = value.into();
        if value.len() > SAFE_DIAGNOSTIC_TEXT_BYTE_LIMIT {
            return Err(SurfaceValueError::TooLong {
                maximum: SAFE_DIAGNOSTIC_TEXT_BYTE_LIMIT,
                observed: value.len(),
            });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for SafeDiagnosticText {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::try_new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// An absolute, lexically normalized path with a lossless UTF-8 wire representation.
///
/// The frozen transparent string wire shape requires every constructed value to
/// serialize without platform-specific byte or wide-string encoding failures.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct CanonicalPath(PathBuf);

impl CanonicalPath {
    pub fn try_new(value: PathBuf) -> Result<Self, SurfaceValueError> {
        if value.to_str().is_none() {
            return Err(SurfaceValueError::InvalidFormat);
        }
        if !value.is_absolute()
            || value
                .components()
                .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
            || value.components().collect::<PathBuf>().as_os_str() != value.as_os_str()
        {
            return Err(SurfaceValueError::NonCanonical);
        }
        Ok(Self(value))
    }

    pub fn as_path(&self) -> &std::path::Path {
        &self.0
    }
}

#[cfg(test)]
pub(crate) fn test_canonical_path(name: &str) -> CanonicalPath {
    CanonicalPath::try_new(std::env::temp_dir().join(name))
        .expect("host temp directory yields a canonical test path")
}

impl<'de> Deserialize<'de> for CanonicalPath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::try_new(PathBuf::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

macro_rules! canonical_string {
    ($name:ident, $validator:ident) => {
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn try_new(value: impl Into<String>) -> Result<Self, SurfaceValueError> {
                let value = value.into();
                $validator(&value)?;
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                Self::try_new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
            }
        }
    };
}

fn validate_uri(value: &str) -> Result<(), SurfaceValueError> {
    if value
        .chars()
        .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(SurfaceValueError::InvalidFormat);
    }
    let (scheme, remainder) = value
        .split_once(':')
        .ok_or(SurfaceValueError::InvalidFormat)?;
    if scheme.is_empty()
        || !scheme.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_lowercase()
                || (index > 0 && (byte.is_ascii_digit() || matches!(byte, b'+' | b'-' | b'.')))
        })
        || remainder.is_empty()
    {
        return Err(SurfaceValueError::NonCanonical);
    }
    if let Some(authority_and_path) = remainder.strip_prefix("//") {
        let authority = authority_and_path
            .split(['/', '?', '#'])
            .next()
            .unwrap_or_default();
        if authority.is_empty() && scheme != "file" {
            return Err(SurfaceValueError::InvalidFormat);
        }
        validate_uri_authority(scheme, authority)?;
    } else if matches!(scheme, "http" | "https" | "ws" | "wss" | "ftp") {
        return Err(SurfaceValueError::InvalidFormat);
    }
    Ok(())
}

fn validate_uri_authority(scheme: &str, authority: &str) -> Result<(), SurfaceValueError> {
    if authority.is_empty() {
        return Ok(());
    }
    let host_and_port = match authority.split_once('@') {
        Some((userinfo, host_and_port)) => {
            if userinfo.is_empty() || host_and_port.is_empty() || host_and_port.contains('@') {
                return Err(SurfaceValueError::InvalidFormat);
            }
            validate_uri_userinfo(userinfo)?;
            host_and_port
        }
        None => authority,
    };
    if host_and_port.is_empty() {
        return Err(SurfaceValueError::InvalidFormat);
    }

    let (host, port) = if let Some(ipv6) = host_and_port.strip_prefix('[') {
        let close = ipv6.find(']').ok_or(SurfaceValueError::InvalidFormat)?;
        let host = &ipv6[..close];
        let suffix = &ipv6[close + 1..];
        let port = if suffix.is_empty() {
            None
        } else {
            Some(
                suffix
                    .strip_prefix(':')
                    .ok_or(SurfaceValueError::InvalidFormat)?,
            )
        };
        let parsed = Ipv6Addr::from_str(host).map_err(|_| SurfaceValueError::InvalidFormat)?;
        if parsed.to_string() != host {
            return Err(SurfaceValueError::NonCanonical);
        }
        (host, port)
    } else {
        let mut parts = host_and_port.rsplitn(2, ':');
        let last = parts.next().unwrap_or_default();
        let before = parts.next();
        let (host, port) = match before {
            Some(host) => (host, Some(last)),
            None => (last, None),
        };
        if host.is_empty() || host.bytes().any(|byte| byte.is_ascii_uppercase()) {
            return Err(SurfaceValueError::NonCanonical);
        }
        if host
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.')
        {
            let parsed = Ipv4Addr::from_str(host).map_err(|_| SurfaceValueError::InvalidFormat)?;
            if parsed.to_string() != host {
                return Err(SurfaceValueError::NonCanonical);
            }
        } else {
            validate_domain(host)?;
        }
        (host, port)
    };

    if host.is_empty() && scheme != "file" {
        return Err(SurfaceValueError::InvalidFormat);
    }
    if let Some(port) = port {
        if port.is_empty()
            || !port.bytes().all(|byte| byte.is_ascii_digit())
            || (port.len() > 1 && port.starts_with('0'))
        {
            return Err(SurfaceValueError::NonCanonical);
        }
        let port = port
            .parse::<u16>()
            .map_err(|_| SurfaceValueError::InvalidFormat)?;
        if matches!(
            (scheme, port),
            ("http", 80) | ("https", 443) | ("ws", 80) | ("wss", 443) | ("ftp", 21)
        ) {
            return Err(SurfaceValueError::NonCanonical);
        }
    }
    Ok(())
}

fn validate_uri_userinfo(value: &str) -> Result<(), SurfaceValueError> {
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'%' {
            let Some(encoded) = bytes.get(index + 1..index + 3) else {
                return Err(SurfaceValueError::InvalidFormat);
            };
            let decode_hex = |byte| match byte {
                b'0'..=b'9' => Some(byte - b'0'),
                b'A'..=b'F' => Some(byte - b'A' + 10),
                _ => None,
            };
            let decoded = decode_hex(encoded[0])
                .zip(decode_hex(encoded[1]))
                .map(|(high, low)| high * 16 + low)
                .ok_or(SurfaceValueError::NonCanonical)?;
            if decoded.is_ascii_alphanumeric() || matches!(decoded, b'-' | b'.' | b'_' | b'~') {
                return Err(SurfaceValueError::NonCanonical);
            }
            index += 3;
            continue;
        }
        if !byte.is_ascii_alphanumeric()
            && !matches!(
                byte,
                b'-' | b'.'
                    | b'_'
                    | b'~'
                    | b'!'
                    | b'$'
                    | b'&'
                    | b'\''
                    | b'('
                    | b')'
                    | b'*'
                    | b'+'
                    | b','
                    | b';'
                    | b'='
                    | b':'
            )
        {
            return Err(SurfaceValueError::InvalidFormat);
        }
        index += 1;
    }
    Ok(())
}

fn validate_mime(value: &str) -> Result<(), SurfaceValueError> {
    if value.contains(';') || value.bytes().any(|byte| byte.is_ascii_uppercase()) {
        return Err(SurfaceValueError::NonCanonical);
    }
    let mut parts = value.split('/');
    let valid_part = |part: &str| {
        !part.is_empty()
            && part.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(
                        byte,
                        b'!' | b'#' | b'$' | b'&' | b'^' | b'_' | b'.' | b'+' | b'-'
                    )
            })
    };
    if !valid_part(parts.next().unwrap_or_default())
        || !valid_part(parts.next().unwrap_or_default())
        || parts.next().is_some()
    {
        return Err(SurfaceValueError::InvalidFormat);
    }
    Ok(())
}

fn validate_domain(value: &str) -> Result<(), SurfaceValueError> {
    if value.is_empty()
        || value.len() > 253
        || value.bytes().any(|byte| byte.is_ascii_uppercase())
        || value.contains(['*', '/', ':'])
        || value.starts_with('.')
        || value.ends_with('.')
    {
        return Err(SurfaceValueError::NonCanonical);
    }
    if !value.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    }) {
        return Err(SurfaceValueError::InvalidFormat);
    }
    Ok(())
}

fn validate_rfc3339_utc(value: &str) -> Result<(), SurfaceValueError> {
    if !value.ends_with('Z') || chrono::DateTime::parse_from_rfc3339(value).is_err() {
        return Err(SurfaceValueError::InvalidFormat);
    }
    Ok(())
}

canonical_string!(CanonicalUri, validate_uri);
canonical_string!(CanonicalMime, validate_mime);
canonical_string!(CanonicalDomainName, validate_domain);
canonical_string!(Rfc3339Timestamp, validate_rfc3339_utc);

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(transparent)]
pub struct FiniteF64(f64);

impl FiniteF64 {
    pub fn try_new(value: f64) -> Result<Self, SurfaceValueError> {
        if !value.is_finite() {
            return Err(SurfaceValueError::NonFinite);
        }
        Ok(Self(value))
    }

    pub fn get(self) -> f64 {
        self.0
    }
}

impl<'de> Deserialize<'de> for FiniteF64 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::try_new(f64::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

macro_rules! scalar_value {
    ($name:ident, $inner:ty) => {
        #[derive(
            Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name($inner);

        impl $name {
            pub const fn new(value: $inner) -> Self {
                Self(value)
            }

            pub const fn get(self) -> $inner {
                self.0
            }
        }
    };
}

scalar_value!(UnixMillis, i64);
scalar_value!(DurationMillis, u64);
scalar_value!(MonotonicTick, u64);
scalar_value!(ByteOffset, u64);
scalar_value!(ByteCount, u64);
scalar_value!(SequenceNumber, u64);
scalar_value!(ThreadOwnerEpoch, u64);
scalar_value!(GoalObjectiveRevision, u32);
scalar_value!(SurfaceGenerationId, u64);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct Revision(u64);

impl Revision {
    pub fn try_new(value: u64) -> Result<Self, SurfaceValueError> {
        if value == 0 {
            return Err(SurfaceValueError::Zero);
        }
        Ok(Self(value))
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

impl<'de> Deserialize<'de> for Revision {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::try_new(u64::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

macro_rules! revision_value {
    ($($name:ident),+ $(,)?) => {$ (
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Revision);

        impl $name {
            pub fn try_new(value: u64) -> Result<Self, SurfaceValueError> {
                Revision::try_new(value).map(Self)
            }

            pub const fn get(self) -> u64 {
                self.0.get()
            }
        }
    )+};
}

revision_value!(
    DurableRevision,
    LiveRevision,
    SessionCatalogRevision,
    McpCatalogRevision,
    InputCatalogRevision,
    WorkflowCatalogRevision,
    SessionMetadataRevision,
    SettingsRevision,
    TrustRevision,
    PolicyEpoch,
    MemoryRevision,
    PinnedContextRevision,
    SessionHealthRevision,
    GoalRevision,
    GoalCatalogRevision,
    GoalOwnerEpoch,
    TaskRevision,
    WorkflowRevision,
    SubagentRevision,
    InteractionRevision,
    ToolInvocationRevision,
    ResponseRouteEpoch,
    CapabilityRevision,
    PlanRevision,
    UsageRevision,
    ContextRevision,
    PinnedFileRevision,
    PinnedUserRevision,
    PinnedSystemRevision,
    ProjectRootMemoryRevision,
    BootstrapCredentialRevision,
    HostLifecycleRevision,
);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Sha256Digest([u8; 32]);

impl Sha256Digest {
    pub const fn new(value: [u8; 32]) -> Self {
        Self(value)
    }

    pub fn digest(value: impl AsRef<[u8]>) -> Self {
        use sha2::{Digest, Sha256};
        Self(Sha256::digest(value).into())
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl Serialize for Sha256Digest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Sha256Digest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        <[u8; 32]>::deserialize(deserializer).map(Self)
    }
}

#[derive(Clone)]
pub struct OpaqueToken([u8; 32]);

impl PartialEq for OpaqueToken {
    fn eq(&self, other: &Self) -> bool {
        self.0
            .iter()
            .zip(other.0.iter())
            .fold(0_u8, |difference, (left, right)| {
                difference | (left ^ right)
            })
            == 0
    }
}

impl Eq for OpaqueToken {}

#[allow(dead_code)]
impl OpaqueToken {
    pub(crate) const fn new(value: [u8; 32]) -> Self {
        Self(value)
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Uuid([u8; 16]);

impl Uuid {
    pub fn try_from_bytes(value: [u8; 16]) -> Result<Self, SurfaceValueError> {
        Ok(Self(value))
    }

    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct UuidV7([u8; 16]);

impl UuidV7 {
    pub fn try_from_bytes(value: [u8; 16]) -> Result<Self, SurfaceValueError> {
        let parsed = uuid::Uuid::from_bytes(value);
        if parsed.get_version_num() != 7 || parsed.get_variant() != uuid::Variant::RFC4122 {
            return Err(SurfaceValueError::WrongUuidKind);
        }
        Ok(Self(value))
    }

    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

macro_rules! uuid_serde {
    ($name:ident) => {
        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                uuid::Uuid::from_bytes(self.0)
                    .hyphenated()
                    .to_string()
                    .serialize(serializer)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                let parsed = uuid::Uuid::parse_str(&value).map_err(serde::de::Error::custom)?;
                if value != parsed.hyphenated().to_string() {
                    return Err(serde::de::Error::custom("UUID is not canonical"));
                }
                Self::try_from_bytes(*parsed.as_bytes()).map_err(serde::de::Error::custom)
            }
        }
    };
}

uuid_serde!(Uuid);
uuid_serde!(UuidV7);

pub type Set<T> = BTreeSet<T>;
pub type Denied = ();
pub type Unit = ();

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct NonEmptyVec<T>(Vec<T>);

impl<T> NonEmptyVec<T> {
    pub fn try_new(value: Vec<T>) -> Result<Self, SurfaceValueError> {
        if value.is_empty() {
            return Err(SurfaceValueError::Empty);
        }
        Ok(Self(value))
    }

    pub fn as_slice(&self) -> &[T] {
        &self.0
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for NonEmptyVec<T> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::try_new(Vec::<T>::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct NonEmptySet<T: Ord>(BTreeSet<T>);

impl<T: Ord> NonEmptySet<T> {
    pub fn try_new(value: BTreeSet<T>) -> Result<Self, SurfaceValueError> {
        if value.is_empty() {
            return Err(SurfaceValueError::Empty);
        }
        Ok(Self(value))
    }

    pub fn as_set(&self) -> &BTreeSet<T> {
        &self.0
    }
}

impl<'de, T> Deserialize<'de> for NonEmptySet<T>
where
    T: Deserialize<'de> + Ord,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::try_new(BTreeSet::<T>::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

macro_rules! uuid_wrapper {
    ($name:ident, $inner:ident) => {
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name($inner);

        impl $name {
            pub fn try_from_bytes(value: [u8; 16]) -> Result<Self, SurfaceValueError> {
                $inner::try_from_bytes(value).map(Self)
            }

            pub const fn as_bytes(&self) -> &[u8; 16] {
                self.0.as_bytes()
            }
        }
    };
}

uuid_wrapper!(HostMonotonicClockId, UuidV7);
uuid_wrapper!(SurfaceThreadId, Uuid);
uuid_wrapper!(SurfaceOperationId, UuidV7);
uuid_wrapper!(SurfaceStreamId, UuidV7);
uuid_wrapper!(SurfaceInteractionId, UuidV7);
uuid_wrapper!(SurfaceAttachmentId, UuidV7);
uuid_wrapper!(SurfaceResponseId, UuidV7);
uuid_wrapper!(SurfaceResponseReceiptId, UuidV7);
uuid_wrapper!(SurfaceEventId, UuidV7);
uuid_wrapper!(SurfaceRequestId, UuidV7);
uuid_wrapper!(SurfaceCommitId, UuidV7);
uuid_wrapper!(SurfaceSettlementId, UuidV7);
uuid_wrapper!(SurfaceFinalizeIntentId, UuidV7);
uuid_wrapper!(SurfaceAdmissionLeaseId, UuidV7);
uuid_wrapper!(SurfaceInputCorrelationId, UuidV7);
uuid_wrapper!(SurfaceCapabilityCallId, UuidV7);
uuid_wrapper!(SurfaceConnectionId, UuidV7);
uuid_wrapper!(HostIncarnation, UuidV7);
uuid_wrapper!(SurfaceIncarnation, UuidV7);
uuid_wrapper!(ContextWindowId, Uuid);

impl ContextWindowId {
    pub fn new() -> Self {
        Self(Uuid::try_from_bytes(*uuid::Uuid::now_v7().as_bytes()).expect("UUIDv7 is a UUID"))
    }

    pub fn initial_for_thread(thread_id: &SurfaceThreadId) -> Self {
        use sha2::{Digest, Sha256};

        let mut hasher = Sha256::new();
        hasher.update(b"orca.context-window.v1");
        hasher.update(thread_id.as_bytes());
        Self::from_digest(hasher.finalize())
    }

    pub fn for_compaction(previous: &ContextWindowId, operation_id: &SurfaceOperationId) -> Self {
        use sha2::{Digest, Sha256};

        let mut hasher = Sha256::new();
        hasher.update(b"orca.context-window.compaction.v1");
        hasher.update(previous.as_bytes());
        hasher.update(operation_id.as_bytes());
        Self::from_digest(hasher.finalize())
    }

    pub fn is_legacy_unspecified(&self) -> bool {
        self.as_bytes().iter().all(|byte| *byte == 0)
    }

    fn from_digest(digest: impl AsRef<[u8]>) -> Self {
        let mut bytes = [0_u8; 16];
        bytes.copy_from_slice(&digest.as_ref()[..16]);
        Self(Uuid::try_from_bytes(bytes).expect("digest bytes form a UUID"))
    }
}

impl Default for ContextWindowId {
    fn default() -> Self {
        Self(Uuid::try_from_bytes([0; 16]).expect("zero bytes form a UUID"))
    }
}

impl SurfaceRequestId {
    pub fn new() -> Self {
        Self(
            UuidV7::try_from_bytes(*uuid::Uuid::now_v7().as_bytes()).expect("generated UUID is v7"),
        )
    }
}

macro_rules! text_id {
    ($($name:ident),+ $(,)?) => {$ (
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(NonEmptyText);

        impl $name {
            pub fn try_new(value: impl Into<String>) -> Result<Self, SurfaceValueError> {
                NonEmptyText::try_new(value).map(Self)
            }

            pub fn as_str(&self) -> &str {
                self.0.as_str()
            }
        }
    )+};
}

text_id!(
    SurfaceToolCallId,
    SurfaceTaskId,
    SurfaceActivityId,
    SurfaceWorkflowRunId,
    SurfaceWorkflowResultId,
    SurfaceSubagentId,
    SurfaceGoalId,
    SurfaceGoalRunId,
    SurfaceGoalOuterTurnId,
    SurfaceGoalIntentId,
    SurfaceRemoteTerminalId,
    SurfaceCatalogEntryId,
);

pub type SurfaceTurnId = TurnId;
pub type SurfaceItemId = ConversationItemId;

macro_rules! opaque_token_wrapper {
    ($($name:ident),+ $(,)?) => {$ (
        #[derive(Clone, Eq, PartialEq)]
        pub struct $name(OpaqueToken);

        #[allow(dead_code)]
        impl $name {
            pub(crate) const fn new(value: [u8; 32]) -> Self {
                Self(OpaqueToken::new(value))
            }

            pub(crate) const fn key_bytes(&self) -> &[u8; 32] {
                &self.0.0
            }
        }
    )+};
}

opaque_token_wrapper!(
    SurfaceResponseToken,
    SurfaceResponseGrantToken,
    SurfaceBackgroundOwnerToken,
    SurfacePublisherPermitId,
);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MonotonicInstant {
    pub clock_id: HostMonotonicClockId,
    pub tick: MonotonicTick,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PinnedContextSourceRevision {
    Memory(MemoryRevision),
    File(PinnedFileRevision),
    User(PinnedUserRevision),
    System(PinnedSystemRevision),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum HostRevisionWitness {
    Memory(MemoryRevision),
    FolderTrust(TrustRevision),
    RuntimeSettings(SettingsRevision),
    SessionCatalog(SessionCatalogRevision),
    SessionMetadata(SessionMetadataRevision),
    HostLifecycle(HostLifecycleRevision),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceUnavailableReason {
    HostShuttingDown,
    ThreadClosing,
    ProjectionDegraded,
    CapacityExceeded,
    RuntimeUnavailable,
}

#[derive(Clone)]
pub struct OptionalProcessLocalCancel(Arc<AtomicBool>);

impl OptionalProcessLocalCancel {
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

pub struct ZeroizingProcessLocalSecret(Vec<u8>);

#[allow(dead_code)]
impl ZeroizingProcessLocalSecret {
    pub(crate) fn new(value: Vec<u8>) -> Self {
        Self(value)
    }
}

fn zeroize_process_local_secret(value: &mut [u8]) {
    for byte in value {
        // SAFETY: `byte` is a valid, uniquely borrowed byte in this owned buffer.
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
    compiler_fence(Ordering::SeqCst);
}

impl Drop for ZeroizingProcessLocalSecret {
    fn drop(&mut self) {
        zeroize_process_local_secret(&mut self.0);
    }
}

#[allow(dead_code)]
#[derive(Clone)]
pub struct SurfaceBoundCaller {
    attachment_id: SurfaceAttachmentId,
    connection_id: Option<SurfaceConnectionId>,
}

#[allow(dead_code)]
impl SurfaceBoundCaller {
    pub(crate) fn new(
        attachment_id: SurfaceAttachmentId,
        connection_id: Option<SurfaceConnectionId>,
    ) -> Self {
        Self {
            attachment_id,
            connection_id,
        }
    }
}

#[allow(dead_code)]
#[derive(Clone)]
pub struct SurfaceHostBoundCaller {
    host_incarnation: HostIncarnation,
    connection_id: Option<SurfaceConnectionId>,
}

#[allow(dead_code)]
impl SurfaceHostBoundCaller {
    pub(crate) fn new(
        host_incarnation: HostIncarnation,
        connection_id: Option<SurfaceConnectionId>,
    ) -> Self {
        Self {
            host_incarnation,
            connection_id,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum AcpRequestId {
    String(NonEmptyText),
    Integer(i64),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceOperationFence {
    pub thread_id: SurfaceThreadId,
    pub thread_owner_epoch: ThreadOwnerEpoch,
    pub operation_id: SurfaceOperationId,
    pub generation_id: SurfaceGenerationId,
}

#[derive(Clone, Eq, PartialEq)]
pub struct SurfaceBackgroundFence {
    pub operation_fence: SurfaceOperationFence,
    pub background_owner_token: SurfaceBackgroundOwnerToken,
}

impl std::fmt::Debug for SurfaceBackgroundFence {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SurfaceBackgroundFence")
            .field("operation_fence", &self.operation_fence)
            .field("background_owner_token", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceGoalFence {
    pub goal_id: SurfaceGoalId,
    pub goal_revision: GoalRevision,
    pub goal_owner_epoch: GoalOwnerEpoch,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SurfaceTaskFence {
    pub task_id: SurfaceTaskId,
    pub task_revision: TaskRevision,
    pub background_owner: Option<SurfaceBackgroundFence>,
}

#[derive(Serialize)]
pub(super) struct CanonicalBackgroundFenceV1<'a> {
    operation_fence: &'a SurfaceOperationFence,
    background_owner_token: &'a [u8; 32],
}

pub(super) fn canonical_background_fence_v1(
    fence: &SurfaceBackgroundFence,
) -> CanonicalBackgroundFenceV1<'_> {
    CanonicalBackgroundFenceV1 {
        operation_fence: &fence.operation_fence,
        background_owner_token: &fence.background_owner_token.0.0,
    }
}

#[derive(Serialize)]
pub(super) struct CanonicalTaskFenceV1<'a> {
    task_id: &'a SurfaceTaskId,
    task_revision: TaskRevision,
    background_owner: Option<CanonicalBackgroundFenceV1<'a>>,
}

pub(super) fn canonical_task_fence_v1(fence: &SurfaceTaskFence) -> CanonicalTaskFenceV1<'_> {
    CanonicalTaskFenceV1 {
        task_id: &fence.task_id,
        task_revision: fence.task_revision,
        background_owner: fence
            .background_owner
            .as_ref()
            .map(canonical_background_fence_v1),
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceWorkflowFence {
    pub workflow_run_id: SurfaceWorkflowRunId,
    pub workflow_revision: WorkflowRevision,
    pub parent: Option<SurfaceOperationFence>,
}

#[derive(Clone, Eq, PartialEq)]
pub enum SurfaceScope {
    Thread,
    Operation {
        operation_id: SurfaceOperationId,
    },
    Generation {
        fence: SurfaceOperationFence,
    },
    Background {
        fence: SurfaceBackgroundFence,
    },
    Goal {
        goal_id: SurfaceGoalId,
        causative_generation: Option<SurfaceOperationFence>,
    },
}

#[derive(Serialize)]
pub(super) enum CanonicalSurfaceScopeV1<'a> {
    Thread,
    Operation {
        operation_id: &'a SurfaceOperationId,
    },
    Generation {
        fence: &'a SurfaceOperationFence,
    },
    Background {
        fence: CanonicalBackgroundFenceV1<'a>,
    },
    Goal {
        goal_id: &'a SurfaceGoalId,
        causative_generation: &'a Option<SurfaceOperationFence>,
    },
}

pub(super) fn canonical_surface_scope_v1(scope: &SurfaceScope) -> CanonicalSurfaceScopeV1<'_> {
    match scope {
        SurfaceScope::Thread => CanonicalSurfaceScopeV1::Thread,
        SurfaceScope::Operation { operation_id } => {
            CanonicalSurfaceScopeV1::Operation { operation_id }
        }
        SurfaceScope::Generation { fence } => CanonicalSurfaceScopeV1::Generation { fence },
        SurfaceScope::Background { fence } => CanonicalSurfaceScopeV1::Background {
            fence: canonical_background_fence_v1(fence),
        },
        SurfaceScope::Goal {
            goal_id,
            causative_generation,
        } => CanonicalSurfaceScopeV1::Goal {
            goal_id,
            causative_generation,
        },
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CommitClass {
    Recorded {
        thread_owner_epoch: ThreadOwnerEpoch,
        durable_revision: DurableRevision,
        commit_id: SurfaceCommitId,
    },
    Ephemeral {
        incarnation: SurfaceIncarnation,
        live_revision: LiveRevision,
        commit_id: SurfaceCommitId,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CursorSourceRevision {
    Recorded { durable_revision: DurableRevision },
    Ephemeral { live_revision: LiveRevision },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceCursor {
    pub thread_id: SurfaceThreadId,
    pub incarnation: SurfaceIncarnation,
    pub next_seq: SequenceNumber,
    pub source_revision: CursorSourceRevision,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum SurfaceCapability {
    ReadSnapshot,
    ReadCatalog,
    SubmitOperation,
    ControlBoundOperation,
    ControlAnyVisibleOperation,
    LegacyCancelCurrent,
    LegacyInterruptResume,
    LegacyJsonlControl,
    RespondGrantedInteraction,
    ManageTask,
    ManageWorkflow,
    ManageGoal,
    ManageThreadSettings,
    ManagePinnedContext,
    RepairThread,
    ReadSessionCatalog,
    ManageSessionCatalog,
    ManageSessionLifecycle,
    ManageMemory,
    ReadHostPolicy,
    ManageFolderTrust,
    ReadHostSettings,
    ManageHostSettings,
    ShutdownHost,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceAttachmentRole {
    Tui,
    Acp,
    Jsonl,
    Headless,
    InternalCompatibility,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceAttachmentGrant {
    pub attachment_id: SurfaceAttachmentId,
    pub host_incarnation: HostIncarnation,
    pub role: SurfaceAttachmentRole,
    pub capabilities: NonEmptySet<SurfaceCapability>,
    pub granted_at: SurfaceCursor,
    pub expires_at: Option<MonotonicInstant>,
}

#[derive(Clone, Eq, PartialEq)]
pub enum SurfacePublisherPermit {
    ActorControl {
        permit_id: SurfacePublisherPermitId,
        thread_id: SurfaceThreadId,
        owner_epoch: ThreadOwnerEpoch,
    },
    Generation {
        permit_id: SurfacePublisherPermitId,
        fence: SurfaceOperationFence,
    },
    Background {
        permit_id: SurfacePublisherPermitId,
        fence: SurfaceBackgroundFence,
    },
    Goal {
        permit_id: SurfacePublisherPermitId,
        goal_fence: SurfaceGoalFence,
        receipt_digest: Sha256Digest,
    },
    Finalizer {
        permit_id: SurfacePublisherPermitId,
        operation_id: SurfaceOperationId,
        finalize_intent_id: SurfaceFinalizeIntentId,
        owner_epoch: ThreadOwnerEpoch,
    },
    Recovery {
        permit_id: SurfacePublisherPermitId,
        current_owner_epoch: ThreadOwnerEpoch,
        historical_fence: SurfaceOperationFence,
    },
}

pub struct ProcessLeaseWitness(());

#[allow(dead_code)]
pub struct ThreadOwnershipLease {
    pub thread_id: SurfaceThreadId,
    pub host_incarnation: HostIncarnation,
    pub owner_epoch: ThreadOwnerEpoch,
    pub witness: ProcessLeaseWitness,
}

#[allow(dead_code)]
pub struct PolicyOwnerLease {
    pub lease_id: UuidV7,
    pub host_incarnation: HostIncarnation,
    pub observed_policy_epoch: PolicyEpoch,
    pub governed_roots: NonEmptySet<CanonicalPath>,
    pub witness: ProcessLeaseWitness,
    pub diagnostic_expires_at: UnixMillis,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SurfaceValueError {
    Empty,
    Zero,
    NonFinite,
    InvalidFormat,
    NonCanonical,
    WrongUuidKind,
    TooLong { maximum: usize, observed: usize },
}

impl fmt::Display for SurfaceValueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for SurfaceValueError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opaque_tokens_compare_without_exposing_bytes() {
        assert!(OpaqueToken::new([1; 32]) == OpaqueToken::new([1; 32]));
        assert!(OpaqueToken::new([1; 32]) != OpaqueToken::new([2; 32]));
    }

    #[test]
    fn process_local_secret_zeroization_clears_every_byte() {
        let mut bytes = vec![1, 2, 3, 4];
        zeroize_process_local_secret(&mut bytes);
        assert_eq!(bytes, [0, 0, 0, 0]);
    }

    #[test]
    fn terminal_wait_cancel_signal_is_one_shot() {
        let signal = OptionalProcessLocalCancel::new();
        assert!(!signal.is_cancelled());
        signal.cancel();
        assert!(signal.is_cancelled());
        signal.cancel();
        assert!(signal.is_cancelled());
    }

    #[test]
    fn initial_context_window_identity_is_stable_per_thread_and_isolated_across_forks() {
        let first = SurfaceThreadId::try_from_bytes([1; 16]).unwrap();
        let second = SurfaceThreadId::try_from_bytes([2; 16]).unwrap();

        assert_eq!(
            ContextWindowId::initial_for_thread(&first),
            ContextWindowId::initial_for_thread(&first)
        );
        assert_ne!(
            ContextWindowId::initial_for_thread(&first),
            ContextWindowId::initial_for_thread(&second)
        );
    }
}
