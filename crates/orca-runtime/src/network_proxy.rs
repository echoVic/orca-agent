use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener as StdTcpListener};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use hickory_resolver::TokioResolver;
use orca_core::config::PermissionProfileNetworkAccess;
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, oneshot};
use tokio::task::JoinSet;
use tokio::time::timeout;

const MAX_PROXY_CONNECTIONS: usize = 32;
const MAX_PROXY_REQUEST_LINE_BYTES: usize = 8 * 1024;
const MAX_PROXY_HEADER_LINE_BYTES: usize = 16 * 1024;
const MAX_PROXY_HEADER_BYTES: usize = 64 * 1024;
const MAX_PROXY_HEADERS: usize = 100;
const MAX_NETWORK_BLOCK_REPORTS: usize = 8;
const DNS_LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);
const UPSTREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const PROXY_IO_IDLE_TIMEOUT: Duration = Duration::from_secs(10);
const OVERLOAD_RESPONSE_TIMEOUT: Duration = Duration::from_millis(250);
const PROXY_STARTUP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeNetworkPolicy {
    domains: HashMap<String, PermissionProfileNetworkAccess>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuntimeNetworkBlockReason {
    Allowlist,
    Denylist,
    NotAllowedLocal,
    Policy,
}

impl RuntimeNetworkBlockReason {
    fn proxy_error(self) -> &'static str {
        match self {
            Self::Allowlist => "blocked-by-allowlist",
            Self::Denylist => "blocked-by-denylist",
            Self::NotAllowedLocal => "blocked-by-policy",
            Self::Policy => "blocked-by-policy",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuntimeNetworkDecision {
    Allow,
    Block(RuntimeNetworkBlockReason),
}

impl RuntimeNetworkPolicy {
    pub fn new(domains: HashMap<String, PermissionProfileNetworkAccess>) -> Self {
        let domains = domains
            .into_iter()
            .map(|(domain, access)| (normalize_host(&domain), access))
            .collect();
        Self { domains }
    }

    pub fn is_empty(&self) -> bool {
        self.domains.is_empty()
    }

    fn access_for_host(&self, host: &str) -> Option<PermissionProfileNetworkAccess> {
        let host = normalize_host(host);
        self.domains.get(&host).copied().or_else(|| {
            self.domains.iter().find_map(|(pattern, access)| {
                domain_pattern_matches(pattern, &host).then_some(*access)
            })
        })
    }

    #[cfg(test)]
    fn allows_host(&self, host: &str) -> bool {
        matches!(self.decision_for_host(host), RuntimeNetworkDecision::Allow)
    }

    fn decision_for_host(&self, host: &str) -> RuntimeNetworkDecision {
        let normalized_host = normalize_host(host);
        match self.access_for_host(host) {
            Some(PermissionProfileNetworkAccess::Deny) => {
                RuntimeNetworkDecision::Block(RuntimeNetworkBlockReason::Denylist)
            }
            Some(PermissionProfileNetworkAccess::Allow) => RuntimeNetworkDecision::Allow,
            None if is_local_or_private_host_literal(&normalized_host) => {
                RuntimeNetworkDecision::Block(RuntimeNetworkBlockReason::NotAllowedLocal)
            }
            None if self
                .domains
                .values()
                .any(|access| matches!(access, PermissionProfileNetworkAccess::Allow)) =>
            {
                RuntimeNetworkDecision::Block(RuntimeNetworkBlockReason::Allowlist)
            }
            None => RuntimeNetworkDecision::Allow,
        }
    }

    fn with_allowed_host(&self, host: &str) -> Self {
        let mut policy = self.clone();
        policy
            .domains
            .insert(normalize_host(host), PermissionProfileNetworkAccess::Allow);
        policy
    }
}

pub struct RuntimeNetworkProxy {
    proxy_url: String,
    shutdown: Option<oneshot::Sender<()>>,
    supervisor_thread: Option<thread::JoinHandle<io::Result<()>>>,
    active_connections: Arc<AtomicUsize>,
    max_connections: usize,
}

impl RuntimeNetworkProxy {
    pub fn start(policy: RuntimeNetworkPolicy) -> io::Result<Self> {
        Self::start_with_block_reporter(policy, None)
    }

    pub fn start_with_block_reporter(
        policy: RuntimeNetworkPolicy,
        block_reporter: Option<mpsc::SyncSender<RuntimeNetworkBlockReport>>,
    ) -> io::Result<Self> {
        Self::start_with_connection_limit(policy, block_reporter, None, MAX_PROXY_CONNECTIONS)
    }

    pub fn start_with_permission_gate(
        policy: RuntimeNetworkPolicy,
        permission_gate: Option<mpsc::SyncSender<RuntimeNetworkBlockRequest>>,
    ) -> io::Result<Self> {
        Self::start_with_connection_limit(policy, None, permission_gate, MAX_PROXY_CONNECTIONS)
    }

    fn start_with_connection_limit(
        policy: RuntimeNetworkPolicy,
        block_reporter: Option<mpsc::SyncSender<RuntimeNetworkBlockReport>>,
        permission_gate: Option<mpsc::SyncSender<RuntimeNetworkBlockRequest>>,
        max_connections: usize,
    ) -> io::Result<Self> {
        if max_connections == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "network proxy connection limit must be positive",
            ));
        }
        let listener = StdTcpListener::bind(("127.0.0.1", 0))?;
        let addr = listener.local_addr()?;
        listener.set_nonblocking(true)?;
        let (shutdown, shutdown_receiver) = oneshot::channel();
        let (startup_sender, startup_receiver) = mpsc::sync_channel(1);
        let active_connections = Arc::new(AtomicUsize::new(0));
        let supervisor_active_connections = Arc::clone(&active_connections);
        let supervisor_thread = thread::Builder::new()
            .name("orca-network-proxy".to_string())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?;
                runtime.block_on(run_proxy_supervisor(
                    listener,
                    Arc::new(policy),
                    block_reporter,
                    permission_gate,
                    max_connections,
                    supervisor_active_connections,
                    shutdown_receiver,
                    startup_sender,
                ))
            })?;

        match startup_receiver.recv_timeout(PROXY_STARTUP_TIMEOUT) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                let _ = shutdown.send(());
                let _ = supervisor_thread.join();
                return Err(error);
            }
            Err(error) => {
                let _ = shutdown.send(());
                let _ = supervisor_thread.join();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("network proxy supervisor failed to start: {error}"),
                ));
            }
        }

        Ok(Self {
            proxy_url: format!("http://{addr}"),
            shutdown: Some(shutdown),
            supervisor_thread: Some(supervisor_thread),
            active_connections,
            max_connections,
        })
    }

    #[cfg(test)]
    fn start_with_connection_limit_for_test(
        policy: RuntimeNetworkPolicy,
        block_reporter: Option<mpsc::SyncSender<RuntimeNetworkBlockReport>>,
        max_connections: usize,
    ) -> io::Result<Self> {
        Self::start_with_connection_limit(policy, block_reporter, None, max_connections)
    }

    pub fn proxy_url(&self) -> &str {
        &self.proxy_url
    }

    pub fn active_connection_count(&self) -> usize {
        self.active_connections.load(Ordering::Acquire)
    }

    pub fn max_connection_count(&self) -> usize {
        self.max_connections
    }

    pub fn shutdown(mut self) -> io::Result<()> {
        self.shutdown_inner()
    }

    fn shutdown_inner(&mut self) -> io::Result<()> {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let Some(supervisor_thread) = self.supervisor_thread.take() else {
            return Ok(());
        };
        match supervisor_thread.join() {
            Ok(result) => result,
            Err(_) => Err(io::Error::other("network proxy supervisor panicked")),
        }
    }
}

impl Drop for RuntimeNetworkProxy {
    fn drop(&mut self) {
        let _ = self.shutdown_inner();
    }
}

pub fn runtime_network_block_channel() -> (
    mpsc::SyncSender<RuntimeNetworkBlockReport>,
    mpsc::Receiver<RuntimeNetworkBlockReport>,
) {
    mpsc::sync_channel(MAX_NETWORK_BLOCK_REPORTS)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeNetworkBlockDecision {
    Allow,
    Deny,
}

pub struct RuntimeNetworkBlockRequest {
    pub report: RuntimeNetworkBlockReport,
    decision: oneshot::Sender<RuntimeNetworkBlockDecision>,
}

impl RuntimeNetworkBlockRequest {
    pub fn resolve(self, decision: RuntimeNetworkBlockDecision) -> io::Result<()> {
        self.decision.send(decision).map_err(|_| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "network permission request is no longer active",
            )
        })
    }
}

pub fn runtime_network_permission_gate_channel() -> (
    mpsc::SyncSender<RuntimeNetworkBlockRequest>,
    mpsc::Receiver<RuntimeNetworkBlockRequest>,
) {
    mpsc::sync_channel(MAX_NETWORK_BLOCK_REPORTS)
}

async fn run_proxy_supervisor(
    listener: StdTcpListener,
    policy: Arc<RuntimeNetworkPolicy>,
    block_reporter: Option<mpsc::SyncSender<RuntimeNetworkBlockReport>>,
    permission_gate: Option<mpsc::SyncSender<RuntimeNetworkBlockRequest>>,
    max_connections: usize,
    active_connections: Arc<AtomicUsize>,
    mut shutdown: oneshot::Receiver<()>,
    startup_sender: mpsc::SyncSender<io::Result<()>>,
) -> io::Result<()> {
    let listener = match TcpListener::from_std(listener) {
        Ok(listener) => listener,
        Err(error) => {
            let _ = startup_sender.send(Err(clone_io_error(&error)));
            return Err(error);
        }
    };
    let resolver = match TokioResolver::builder_tokio() {
        Ok(builder) => Arc::new(builder.build()),
        Err(error) => {
            let error = io::Error::other(format!("failed to initialize DNS resolver: {error}"));
            let _ = startup_sender.send(Err(clone_io_error(&error)));
            return Err(error);
        }
    };
    let _ = startup_sender.send(Ok(()));
    let permits = Arc::new(Semaphore::new(max_connections));
    let mut connections = JoinSet::new();

    let result = loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => break Ok(()),
            completed = connections.join_next(), if !connections.is_empty() => {
                let _ = completed;
            }
            accepted = listener.accept() => {
                let (mut stream, _) = match accepted {
                    Ok(accepted) => accepted,
                    Err(error) => break Err(error),
                };
                let permit = match Arc::clone(&permits).try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => {
                        let _ = timeout(
                            OVERLOAD_RESPONSE_TIMEOUT,
                            write_connection_limit_response(&mut stream),
                        )
                        .await;
                        continue;
                    }
                };
                let policy = Arc::clone(&policy);
                let resolver = Arc::clone(&resolver);
                let reporter = block_reporter.clone();
                let permission_gate = permission_gate.clone();
                let active_connections = Arc::clone(&active_connections);
                active_connections.fetch_add(1, Ordering::AcqRel);
                let active = ActiveConnectionGuard(active_connections);
                connections.spawn(async move {
                    let _permit = permit;
                    let _active = active;
                    let _ = handle_proxy_connection(
                        stream,
                        &policy,
                        reporter.as_ref(),
                        permission_gate.as_ref(),
                        &resolver,
                    )
                    .await;
                });
            }
        }
    };

    connections.abort_all();
    while connections.join_next().await.is_some() {}
    result
}

struct ActiveConnectionGuard(Arc<AtomicUsize>);

impl Drop for ActiveConnectionGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn clone_io_error(error: &io::Error) -> io::Error {
    io::Error::new(error.kind(), error.to_string())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeNetworkBlockReport {
    pub host: String,
    pub error: &'static str,
}

async fn handle_proxy_connection(
    mut client: TcpStream,
    policy: &RuntimeNetworkPolicy,
    block_reporter: Option<&mpsc::SyncSender<RuntimeNetworkBlockReport>>,
    permission_gate: Option<&mpsc::SyncSender<RuntimeNetworkBlockRequest>>,
    resolver: &TokioResolver,
) -> io::Result<()> {
    let request = {
        let mut reader = BufReader::new(&mut client);
        match read_proxy_request(&mut reader).await {
            Ok(request) => request,
            Err(ProxyFrameError::TooLarge) => {
                return write_header_too_large_response(&mut client).await;
            }
            Err(ProxyFrameError::Invalid) => {
                return write_bad_request_response(&mut client).await;
            }
            Err(ProxyFrameError::Io(error)) => return Err(error),
        }
    };
    let Some(request) = request else {
        return Ok(());
    };
    let mut parts = request.request_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    let version = parts.next().unwrap_or("HTTP/1.1");
    let host = proxy_target_host(target)
        .or_else(|| header_host(&request.headers))
        .unwrap_or_default();
    if host.is_empty() {
        write_forbidden(&mut client, RuntimeNetworkBlockReason::Policy, None).await?;
        return Ok(());
    }
    let mut approved_policy = None;
    if let RuntimeNetworkDecision::Block(reason) = policy.decision_for_host(&host) {
        report_block(block_reporter, &host, reason);
        let decision = request_network_permission(permission_gate, &host, reason).await;
        if reason != RuntimeNetworkBlockReason::Denylist
            && decision == Some(RuntimeNetworkBlockDecision::Allow)
        {
            approved_policy = Some(policy.with_allowed_host(&host));
        } else {
            write_forbidden(&mut client, reason, Some(&host)).await?;
            return Ok(());
        }
    }
    let policy = approved_policy.as_ref().unwrap_or(policy);

    let proxy_result = if method.eq_ignore_ascii_case("CONNECT") {
        proxy_connect(&mut client, target, policy, resolver).await
    } else {
        proxy_http(
            &mut client,
            method,
            target,
            version,
            &request.headers,
            policy,
            resolver,
        )
        .await
    };
    if matches!(proxy_result, Err(ref error) if error.kind() == io::ErrorKind::PermissionDenied) {
        report_block(
            block_reporter,
            &host,
            RuntimeNetworkBlockReason::NotAllowedLocal,
        );
        write_forbidden(
            &mut client,
            RuntimeNetworkBlockReason::NotAllowedLocal,
            Some(&host),
        )
        .await?;
        return Ok(());
    }
    proxy_result
}

async fn request_network_permission(
    permission_gate: Option<&mpsc::SyncSender<RuntimeNetworkBlockRequest>>,
    host: &str,
    reason: RuntimeNetworkBlockReason,
) -> Option<RuntimeNetworkBlockDecision> {
    let permission_gate = permission_gate?;
    let (decision, receiver) = oneshot::channel();
    permission_gate
        .try_send(RuntimeNetworkBlockRequest {
            report: RuntimeNetworkBlockReport {
                host: normalize_host(host),
                error: reason.proxy_error(),
            },
            decision,
        })
        .ok()?;
    receiver.await.ok()
}

fn report_block(
    block_reporter: Option<&mpsc::SyncSender<RuntimeNetworkBlockReport>>,
    host: &str,
    reason: RuntimeNetworkBlockReason,
) {
    let Some(block_reporter) = block_reporter else {
        return;
    };
    let _ = block_reporter.try_send(RuntimeNetworkBlockReport {
        host: normalize_host(host),
        error: reason.proxy_error(),
    });
}

struct ProxyRequest {
    request_line: String,
    headers: Vec<String>,
}

enum ProxyFrameError {
    Io(io::Error),
    TooLarge,
    Invalid,
}

async fn read_proxy_request<R>(reader: &mut R) -> Result<Option<ProxyRequest>, ProxyFrameError>
where
    R: AsyncBufRead + Unpin,
{
    let Some(request_line) = read_bounded_utf8_line(reader, MAX_PROXY_REQUEST_LINE_BYTES).await?
    else {
        return Ok(None);
    };
    let request_line = request_line.trim_end_matches(['\r', '\n']).to_string();
    let mut header_bytes = 0_usize;
    let mut headers = Vec::new();
    loop {
        let Some(line) = read_bounded_utf8_line(reader, MAX_PROXY_HEADER_LINE_BYTES).await? else {
            break;
        };
        if line == "\r\n" || line == "\n" {
            break;
        }
        header_bytes = header_bytes
            .checked_add(line.len())
            .ok_or(ProxyFrameError::TooLarge)?;
        if header_bytes > MAX_PROXY_HEADER_BYTES || headers.len() >= MAX_PROXY_HEADERS {
            return Err(ProxyFrameError::TooLarge);
        }
        headers.push(line);
    }
    Ok(Some(ProxyRequest {
        request_line,
        headers,
    }))
}

async fn read_bounded_utf8_line<R>(
    reader: &mut R,
    max_bytes: usize,
) -> Result<Option<String>, ProxyFrameError>
where
    R: AsyncBufRead + Unpin,
{
    let mut line = Vec::with_capacity(max_bytes.min(1024));
    loop {
        let available = timeout(PROXY_IO_IDLE_TIMEOUT, reader.fill_buf())
            .await
            .map_err(|_| {
                ProxyFrameError::Io(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "network proxy request read timed out",
                ))
            })?
            .map_err(ProxyFrameError::Io)?;
        if available.is_empty() {
            if line.is_empty() {
                return Ok(None);
            }
            break;
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|position| position + 1)
            .unwrap_or(available.len());
        if line.len().saturating_add(take) > max_bytes {
            return Err(ProxyFrameError::TooLarge);
        }
        let finished = available[..take].last() == Some(&b'\n');
        line.extend_from_slice(&available[..take]);
        reader.consume(take);
        if finished {
            break;
        }
    }
    String::from_utf8(line)
        .map(Some)
        .map_err(|_| ProxyFrameError::Invalid)
}

async fn proxy_http(
    client: &mut TcpStream,
    method: &str,
    target: &str,
    version: &str,
    headers: &[String],
    policy: &RuntimeNetworkPolicy,
    resolver: &TokioResolver,
) -> io::Result<()> {
    let Some((host, port, path)) = parse_http_target(target, headers) else {
        write_forbidden(client, RuntimeNetworkBlockReason::Policy, None).await?;
        return Ok(());
    };
    let mut upstream = connect_checked_resolved(&host, port, policy, resolver).await?;
    write_all_with_idle(
        &mut upstream,
        format!("{method} {path} {version}\r\n").as_bytes(),
    )
    .await?;
    for header in headers {
        if header
            .split_once(':')
            .map(|(name, _)| name.eq_ignore_ascii_case("proxy-connection"))
            .unwrap_or(false)
        {
            continue;
        }
        write_all_with_idle(&mut upstream, header.as_bytes()).await?;
    }
    write_all_with_idle(&mut upstream, b"\r\n").await?;
    copy_with_idle(&mut upstream, client).await?;
    Ok(())
}

async fn proxy_connect(
    client: &mut TcpStream,
    target: &str,
    policy: &RuntimeNetworkPolicy,
    resolver: &TokioResolver,
) -> io::Result<()> {
    let (host, port) = split_host_port(target, 443);
    let upstream = connect_checked_resolved(&host, port, policy, resolver).await?;
    write_all_with_idle(client, b"HTTP/1.1 200 Connection Established\r\n\r\n").await?;
    let (mut client_read, mut client_write) = client.split();
    let (mut upstream_read, mut upstream_write) = upstream.into_split();
    tokio::try_join!(
        copy_with_idle(&mut client_read, &mut upstream_write),
        copy_with_idle(&mut upstream_read, &mut client_write),
    )?;
    Ok(())
}

async fn connect_checked_resolved(
    host: &str,
    port: u16,
    policy: &RuntimeNetworkPolicy,
    resolver: &TokioResolver,
) -> io::Result<TcpStream> {
    let resolved = if let Ok(ip) = host.parse::<IpAddr>() {
        vec![SocketAddr::new(ip, port)]
    } else {
        let lookup = timeout(DNS_LOOKUP_TIMEOUT, resolver.lookup_ip(host))
            .await
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("network proxy DNS lookup timed out for {host}"),
                )
            })?
            .map_err(|error| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("network proxy DNS lookup failed for {host}: {error}"),
                )
            })?;
        lookup
            .iter()
            .map(|ip| SocketAddr::new(ip, port))
            .collect::<Vec<_>>()
    };
    let addrs = checked_socket_addrs(host, policy, resolved)?;
    let mut last_error = None;
    for addr in addrs {
        match timeout(UPSTREAM_CONNECT_TIMEOUT, TcpStream::connect(addr)).await {
            Ok(Ok(stream)) => return Ok(stream),
            Ok(Err(error)) => last_error = Some(error),
            Err(_) => {
                last_error = Some(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("network proxy connect timed out for {addr}"),
                ));
            }
        }
    }
    Err(last_error.unwrap_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "network target did not resolve")
    }))
}

async fn write_all_with_idle<W>(writer: &mut W, bytes: &[u8]) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    timeout(PROXY_IO_IDLE_TIMEOUT, writer.write_all(bytes))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "network proxy write timed out"))?
}

async fn copy_with_idle<R, W>(reader: &mut R, writer: &mut W) -> io::Result<u64>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buffer = [0_u8; 8 * 1024];
    let mut copied = 0_u64;
    loop {
        let read = timeout(PROXY_IO_IDLE_TIMEOUT, reader.read(&mut buffer))
            .await
            .map_err(|_| {
                io::Error::new(io::ErrorKind::TimedOut, "network proxy read timed out")
            })??;
        if read == 0 {
            let _ = timeout(PROXY_IO_IDLE_TIMEOUT, writer.shutdown()).await;
            return Ok(copied);
        }
        write_all_with_idle(writer, &buffer[..read]).await?;
        copied = copied.saturating_add(read as u64);
    }
}

fn checked_socket_addrs<I>(
    host: &str,
    policy: &RuntimeNetworkPolicy,
    resolved: I,
) -> io::Result<Vec<SocketAddr>>
where
    I: IntoIterator<Item = SocketAddr>,
{
    let normalized_host = normalize_host(host);
    let allow_explicit_local_literal = is_local_or_private_host_literal(&normalized_host)
        && matches!(
            policy.access_for_host(&normalized_host),
            Some(PermissionProfileNetworkAccess::Allow)
        );
    let mut addrs = Vec::new();
    for addr in resolved {
        if is_local_or_private_ip(addr.ip()) && !allow_explicit_local_literal {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "network target rejected by policy",
            ));
        }
        addrs.push(addr);
    }
    if addrs.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "network target did not resolve",
        ));
    }
    Ok(addrs)
}

async fn write_connection_limit_response(client: &mut TcpStream) -> io::Result<()> {
    write_static_response(
        client,
        b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nx-proxy-error: connection-limit\r\nconnection: close\r\n\r\n",
    )
    .await
}

async fn write_header_too_large_response(client: &mut TcpStream) -> io::Result<()> {
    write_static_response(
        client,
        b"HTTP/1.1 431 Request Header Fields Too Large\r\ncontent-length: 0\r\nx-proxy-error: request-too-large\r\nconnection: close\r\n\r\n",
    )
    .await
}

async fn write_bad_request_response(client: &mut TcpStream) -> io::Result<()> {
    write_static_response(
        client,
        b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\nx-proxy-error: invalid-request\r\nconnection: close\r\n\r\n",
    )
    .await
}

async fn write_static_response(client: &mut TcpStream, response: &[u8]) -> io::Result<()> {
    write_all_with_idle(client, response).await?;
    client.shutdown().await
}

async fn write_forbidden(
    client: &mut TcpStream,
    reason: RuntimeNetworkBlockReason,
    host: Option<&str>,
) -> io::Result<()> {
    let host_header = host
        .map(normalize_host)
        .filter(|host| !host.is_empty())
        .map(|host| format!("x-proxy-host: {host}\r\n"))
        .unwrap_or_default();
    write_static_response(
        client,
        format!(
            "HTTP/1.1 403 Forbidden\r\ncontent-length: 0\r\nx-proxy-error: {}\r\n{host_header}\r\n",
            reason.proxy_error(),
        )
        .as_bytes(),
    )
    .await
}

fn parse_http_target(target: &str, headers: &[String]) -> Option<(String, u16, String)> {
    if let Some(rest) = target.strip_prefix("http://") {
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        let (host, port) = split_host_port(authority, 80);
        return Some((host, port, format!("/{path}")));
    }
    let host = header_host(headers)?;
    let (host, port) = split_host_port(&host, 80);
    Some((host, port, target.to_string()))
}

fn proxy_target_host(target: &str) -> Option<String> {
    if let Some(rest) = target.strip_prefix("http://") {
        return Some(rest.split('/').next().unwrap_or_default().to_string());
    }
    if let Some(rest) = target.strip_prefix("https://") {
        return Some(rest.split('/').next().unwrap_or_default().to_string());
    }
    target.contains(':').then(|| target.to_string())
}

fn header_host(headers: &[String]) -> Option<String> {
    headers.iter().find_map(|header| {
        let (name, value) = header.split_once(':')?;
        name.eq_ignore_ascii_case("host")
            .then(|| value.trim().to_string())
    })
}

fn split_host_port(authority: &str, default_port: u16) -> (String, u16) {
    let authority = authority.trim().trim_matches(['[', ']']);
    if let Some((host, port)) = authority.rsplit_once(':')
        && let Ok(port) = port.parse::<u16>()
    {
        return (host.trim_matches(['[', ']']).to_string(), port);
    }
    (authority.to_string(), default_port)
}

fn normalize_host(host: &str) -> String {
    split_host_port(host, 0)
        .0
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

fn is_local_or_private_host_literal(host: &str) -> bool {
    if host == "localhost" {
        return true;
    }
    host.parse::<IpAddr>()
        .map(is_local_or_private_ip)
        .unwrap_or(false)
}

fn is_local_or_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_local_or_private_ipv4(ip),
        IpAddr::V6(ip) => is_local_or_private_ipv6(ip),
    }
}

fn is_local_or_private_ipv4(ip: Ipv4Addr) -> bool {
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_multicast()
        || ip.is_broadcast()
        || ipv4_in_cidr(ip, [0, 0, 0, 0], 8)
        || ipv4_in_cidr(ip, [100, 64, 0, 0], 10)
        || ipv4_in_cidr(ip, [192, 0, 0, 0], 24)
        || ipv4_in_cidr(ip, [192, 0, 2, 0], 24)
        || ipv4_in_cidr(ip, [198, 18, 0, 0], 15)
        || ipv4_in_cidr(ip, [198, 51, 100, 0], 24)
        || ipv4_in_cidr(ip, [203, 0, 113, 0], 24)
        || ipv4_in_cidr(ip, [240, 0, 0, 0], 4)
}

fn ipv4_in_cidr(ip: Ipv4Addr, base: [u8; 4], prefix: u8) -> bool {
    let ip = u32::from(ip);
    let base = u32::from(Ipv4Addr::from(base));
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    (ip & mask) == (base & mask)
}

fn is_local_or_private_ipv6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_local_or_private_ipv4(v4) || ip.is_loopback();
    }
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || ip.segments()[0] & 0xfe00 == 0xfc00
        || ip.segments()[0] & 0xffc0 == 0xfe80
}

fn domain_pattern_matches(pattern: &str, host: &str) -> bool {
    if let Some(domain) = pattern.strip_prefix("**.") {
        return host == domain || host.ends_with(&format!(".{domain}"));
    }
    if let Some(domain) = pattern.strip_prefix("*.") {
        return host.ends_with(&format!(".{domain}")) && host != domain;
    }
    pattern == host
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc::RecvTimeoutError;
    use std::time::Instant;

    fn proxy_addr(proxy: &RuntimeNetworkProxy) -> String {
        proxy
            .proxy_url()
            .strip_prefix("http://")
            .expect("proxy url prefix")
            .to_string()
    }

    fn wait_for_active_connections(proxy: &RuntimeNetworkProxy, expected: usize) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if proxy.active_connection_count() == expected {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!(
            "expected {expected} active proxy connections, observed {}",
            proxy.active_connection_count()
        );
    }

    #[test]
    fn runtime_network_proxy_stops_accepting_after_drop() {
        let proxy = RuntimeNetworkProxy::start(RuntimeNetworkPolicy::new(HashMap::from([(
            "127.0.0.1".to_string(),
            PermissionProfileNetworkAccess::Allow,
        )])))
        .expect("start proxy");
        let addr = proxy
            .proxy_url()
            .strip_prefix("http://")
            .expect("proxy url prefix")
            .to_string();

        drop(proxy);

        for _ in 0..20 {
            if TcpStream::connect(&addr).is_err() {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("proxy still accepts connections after drop");
    }

    #[test]
    fn runtime_network_proxy_rejects_connections_above_limit_without_spawning_workers() {
        let proxy = RuntimeNetworkProxy::start_with_connection_limit_for_test(
            RuntimeNetworkPolicy::new(HashMap::new()),
            None,
            1,
        )
        .expect("start proxy");
        let addr = proxy_addr(&proxy);
        let _held = TcpStream::connect(&addr).expect("connect held client");
        wait_for_active_connections(&proxy, 1);

        let mut rejected = TcpStream::connect(&addr).expect("connect rejected client");
        rejected
            .set_read_timeout(Some(Duration::from_secs(1)))
            .expect("set rejected read timeout");
        let mut response = String::new();
        rejected
            .read_to_string(&mut response)
            .expect("read overload response");

        assert!(response.starts_with("HTTP/1.1 503 Service Unavailable"));
        assert!(response.contains("x-proxy-error: connection-limit"));
        assert_eq!(proxy.active_connection_count(), 1);
    }

    #[test]
    fn runtime_network_proxy_drop_cancels_and_joins_stalled_connection() {
        let proxy = RuntimeNetworkProxy::start_with_connection_limit_for_test(
            RuntimeNetworkPolicy::new(HashMap::new()),
            None,
            1,
        )
        .expect("start proxy");
        let active_connections = Arc::clone(&proxy.active_connections);
        let addr = proxy_addr(&proxy);
        let mut client = TcpStream::connect(&addr).expect("connect stalled client");
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .expect("set client read timeout");
        wait_for_active_connections(&proxy, 1);

        let started = Instant::now();
        drop(proxy);

        assert!(
            started.elapsed() < Duration::from_secs(1),
            "proxy drop should not wait for the connection idle deadline"
        );
        assert_eq!(active_connections.load(Ordering::Acquire), 0);
        let mut byte = [0_u8; 1];
        assert!(
            matches!(client.read(&mut byte), Ok(0) | Err(_)),
            "proxy shutdown should close the stalled client socket"
        );
    }

    #[test]
    fn runtime_network_proxy_drop_closes_owned_connect_tunnel() {
        let upstream = TcpListener::bind(("127.0.0.1", 0)).expect("bind upstream");
        let upstream_port = upstream.local_addr().expect("upstream addr").port();
        let (accepted_tx, accepted_rx) = mpsc::sync_channel(1);
        let (closed_tx, closed_rx) = mpsc::sync_channel(1);
        let upstream_worker = thread::spawn(move || {
            let (mut stream, _) = upstream.accept().expect("accept upstream tunnel");
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("set upstream read timeout");
            accepted_tx.send(()).expect("report upstream accepted");
            let mut byte = [0_u8; 1];
            let closed = matches!(stream.read(&mut byte), Ok(0) | Err(_));
            closed_tx.send(closed).expect("report upstream closed");
        });
        let proxy = RuntimeNetworkProxy::start(RuntimeNetworkPolicy::new(HashMap::from([(
            "127.0.0.1".to_string(),
            PermissionProfileNetworkAccess::Allow,
        )])))
        .expect("start proxy");
        let mut client = TcpStream::connect(proxy_addr(&proxy)).expect("connect proxy");
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .expect("set tunnel read timeout");
        write!(
            client,
            "CONNECT 127.0.0.1:{upstream_port} HTTP/1.1\r\nHost: 127.0.0.1:{upstream_port}\r\n\r\n"
        )
        .expect("write CONNECT request");
        let mut response = [0_u8; 128];
        let read = client.read(&mut response).expect("read CONNECT response");
        assert!(String::from_utf8_lossy(&response[..read]).starts_with("HTTP/1.1 200"));
        accepted_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("upstream accepted tunnel");
        wait_for_active_connections(&proxy, 1);

        drop(proxy);

        assert_eq!(
            closed_rx.recv_timeout(Duration::from_secs(1)),
            Ok(true),
            "proxy owner should close the upstream tunnel before returning"
        );
        upstream_worker.join().expect("join upstream worker");
    }

    #[test]
    fn runtime_network_proxy_rejects_oversized_newline_free_request_line() {
        let proxy = RuntimeNetworkProxy::start(RuntimeNetworkPolicy::new(HashMap::new()))
            .expect("start proxy");
        let mut client = TcpStream::connect(proxy_addr(&proxy)).expect("connect proxy");
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .expect("set read timeout");
        client
            .write_all(&vec![b'X'; MAX_PROXY_REQUEST_LINE_BYTES + 1])
            .expect("write oversized request line");
        let mut response = String::new();
        client
            .read_to_string(&mut response)
            .expect("read framing response");

        assert!(response.starts_with("HTTP/1.1 431 Request Header Fields Too Large"));
    }

    #[test]
    fn runtime_network_proxy_block_report_queue_is_bounded_and_nonblocking() {
        let (reporter, reports) = runtime_network_block_channel();
        let proxy = RuntimeNetworkProxy::start_with_block_reporter(
            RuntimeNetworkPolicy::new(HashMap::from([(
                "allowed.example.com".to_string(),
                PermissionProfileNetworkAccess::Allow,
            )])),
            Some(reporter),
        )
        .expect("start proxy");
        let addr = proxy_addr(&proxy);

        for _ in 0..(MAX_NETWORK_BLOCK_REPORTS * 3) {
            let mut client = TcpStream::connect(&addr).expect("connect blocked client");
            client
                .write_all(
                    b"GET http://blocked.example.com/ HTTP/1.1\r\nHost: blocked.example.com\r\n\r\n",
                )
                .expect("write blocked request");
            let mut response = String::new();
            client
                .read_to_string(&mut response)
                .expect("read blocked response");
            assert!(response.starts_with("HTTP/1.1 403 Forbidden"));
        }
        wait_for_active_connections(&proxy, 0);

        let retained = reports.try_iter().count();
        assert_eq!(retained, MAX_NETWORK_BLOCK_REPORTS);
        assert_eq!(reports.try_recv(), Err(mpsc::TryRecvError::Empty));
        assert_ne!(
            reports.recv_timeout(Duration::from_millis(10)),
            Err(RecvTimeoutError::Disconnected),
            "report queue remains owned until the proxy is dropped"
        );
    }

    #[test]
    fn permission_gate_resumes_the_same_blocked_proxy_connection() {
        let upstream = TcpListener::bind(("127.0.0.1", 0)).expect("bind upstream");
        let upstream_port = upstream.local_addr().expect("upstream address").port();
        let server = thread::spawn(move || {
            let (mut stream, _) = upstream.accept().expect("accept approved request");
            let mut reader = BufReader::new(stream.try_clone().expect("clone upstream stream"));
            let mut line = String::new();
            while reader.read_line(&mut line).expect("read request") != 0 {
                if line == "\r\n" || line == "\n" {
                    break;
                }
                line.clear();
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 7\r\n\r\nresumed")
                .expect("write upstream response");
        });
        let (gate_sender, gate_receiver) = runtime_network_permission_gate_channel();
        let proxy = RuntimeNetworkProxy::start_with_permission_gate(
            RuntimeNetworkPolicy::new(HashMap::from([(
                "api.example.com".to_string(),
                PermissionProfileNetworkAccess::Allow,
            )])),
            Some(gate_sender),
        )
        .expect("start gated proxy");
        let mut client = TcpStream::connect(proxy_addr(&proxy)).expect("connect to proxy");
        write!(
            client,
            "GET http://127.0.0.1:{upstream_port}/ HTTP/1.1\r\nHost: 127.0.0.1:{upstream_port}\r\nConnection: close\r\n\r\n"
        )
        .expect("write blocked request");

        let request = gate_receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("permission gate request");
        assert_eq!(request.report.host, "127.0.0.1");
        assert_eq!(request.report.error, "blocked-by-policy");
        request
            .resolve(RuntimeNetworkBlockDecision::Allow)
            .expect("allow blocked request");

        let mut response = String::new();
        client
            .read_to_string(&mut response)
            .expect("read resumed response");
        assert!(response.contains("200 OK"));
        assert!(response.ends_with("resumed"));
        server.join().expect("upstream server");
    }

    #[test]
    fn permission_gate_cannot_override_an_explicit_denylist() {
        let (gate_sender, gate_receiver) = runtime_network_permission_gate_channel();
        let proxy = RuntimeNetworkProxy::start_with_permission_gate(
            RuntimeNetworkPolicy::new(HashMap::from([(
                "blocked.example.com".to_string(),
                PermissionProfileNetworkAccess::Deny,
            )])),
            Some(gate_sender),
        )
        .expect("start gated proxy");
        let mut client = TcpStream::connect(proxy_addr(&proxy)).expect("connect to proxy");
        write!(
            client,
            "GET http://blocked.example.com/ HTTP/1.1\r\nHost: blocked.example.com\r\nConnection: close\r\n\r\n"
        )
        .expect("write denied request");

        let request = gate_receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("denylist report");
        assert_eq!(request.report.host, "blocked.example.com");
        assert_eq!(request.report.error, "blocked-by-denylist");
        request
            .resolve(RuntimeNetworkBlockDecision::Allow)
            .expect("resolve request");

        let mut response = String::new();
        client
            .read_to_string(&mut response)
            .expect("read denied response");
        assert!(response.contains("403 Forbidden"));
    }

    #[test]
    fn runtime_network_policy_matches_scoped_domain_patterns() {
        let policy = RuntimeNetworkPolicy::new(HashMap::from([
            (
                "*.example.com".to_string(),
                PermissionProfileNetworkAccess::Allow,
            ),
            (
                "**.blocked.test".to_string(),
                PermissionProfileNetworkAccess::Deny,
            ),
        ]));

        assert!(policy.allows_host("api.example.com"));
        assert!(!policy.allows_host("example.com"));
        assert!(!policy.allows_host("blocked.test"));
        assert!(!policy.allows_host("api.blocked.test"));
    }

    #[test]
    fn runtime_network_policy_blocks_allowlist_misses_when_allow_entries_exist() {
        let policy = RuntimeNetworkPolicy::new(HashMap::from([(
            "api.example.com".to_string(),
            PermissionProfileNetworkAccess::Allow,
        )]));

        assert!(policy.allows_host("api.example.com"));
        assert!(!policy.allows_host("other.example.com"));
    }

    #[test]
    fn runtime_network_policy_allows_misses_when_policy_only_has_denies() {
        let policy = RuntimeNetworkPolicy::new(HashMap::from([(
            "blocked.example.com".to_string(),
            PermissionProfileNetworkAccess::Deny,
        )]));

        assert!(!policy.allows_host("blocked.example.com"));
        assert!(policy.allows_host("api.example.com"));
    }

    #[test]
    fn runtime_network_proxy_reports_denylist_blocks() {
        let proxy = RuntimeNetworkProxy::start(RuntimeNetworkPolicy::new(HashMap::from([
            (
                "api.example.com".to_string(),
                PermissionProfileNetworkAccess::Allow,
            ),
            (
                "blocked.example.com".to_string(),
                PermissionProfileNetworkAccess::Deny,
            ),
        ])))
        .expect("start proxy");
        let addr = proxy
            .proxy_url()
            .strip_prefix("http://")
            .expect("proxy url prefix")
            .to_string();
        let mut stream = TcpStream::connect(addr).expect("connect proxy");

        stream
            .write_all(
                b"GET http://blocked.example.com/ HTTP/1.1\r\nHost: blocked.example.com\r\n\r\n",
            )
            .expect("write request");
        let mut response = String::new();
        std::io::Read::read_to_string(&mut stream, &mut response).expect("read response");

        assert!(response.starts_with("HTTP/1.1 403 Forbidden"));
        assert!(
            response.contains("x-proxy-error: blocked-by-denylist"),
            "response should identify denylist block: {response:?}"
        );
    }

    #[test]
    fn runtime_network_proxy_reports_allowlist_misses() {
        let proxy = RuntimeNetworkProxy::start(RuntimeNetworkPolicy::new(HashMap::from([(
            "api.example.com".to_string(),
            PermissionProfileNetworkAccess::Allow,
        )])))
        .expect("start proxy");
        let addr = proxy
            .proxy_url()
            .strip_prefix("http://")
            .expect("proxy url prefix")
            .to_string();
        let mut stream = TcpStream::connect(addr).expect("connect proxy");

        stream
            .write_all(b"GET http://other.example.com/ HTTP/1.1\r\nHost: other.example.com\r\n\r\n")
            .expect("write request");
        let mut response = String::new();
        std::io::Read::read_to_string(&mut stream, &mut response).expect("read response");

        assert!(response.starts_with("HTTP/1.1 403 Forbidden"));
        assert!(
            response.contains("x-proxy-error: blocked-by-allowlist"),
            "response should identify allowlist block: {response:?}"
        );
        assert!(
            response.contains("x-proxy-host: other.example.com"),
            "response should identify blocked host for permission attribution: {response:?}"
        );
    }

    #[test]
    fn runtime_network_proxy_blocks_loopback_targets_without_explicit_allowlist() {
        let proxy = RuntimeNetworkProxy::start(RuntimeNetworkPolicy::new(HashMap::from([(
            "blocked.example.com".to_string(),
            PermissionProfileNetworkAccess::Deny,
        )])))
        .expect("start proxy");
        let addr = proxy
            .proxy_url()
            .strip_prefix("http://")
            .expect("proxy url prefix")
            .to_string();
        let mut stream = TcpStream::connect(addr).expect("connect proxy");

        stream
            .write_all(b"GET http://127.0.0.1/ HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .expect("write request");
        let mut response = String::new();
        std::io::Read::read_to_string(&mut stream, &mut response).expect("read response");

        assert!(response.starts_with("HTTP/1.1 403 Forbidden"));
        assert!(
            response.contains("x-proxy-error: blocked-by-policy"),
            "response should block local network targets by policy: {response:?}"
        );
    }

    #[test]
    fn runtime_network_proxy_blocks_resolved_private_targets_without_explicit_allowlist() {
        let policy = RuntimeNetworkPolicy::new(HashMap::from([(
            "blocked.example.com".to_string(),
            PermissionProfileNetworkAccess::Deny,
        )]));
        let resolved = vec!["127.0.0.1:80".parse().expect("socket addr")];

        let error = checked_socket_addrs("private.test", &policy, resolved)
            .expect_err("resolved private target should be blocked");

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn runtime_network_proxy_blocks_allowlisted_domains_that_resolve_private() {
        let policy = RuntimeNetworkPolicy::new(HashMap::from([(
            "private.test".to_string(),
            PermissionProfileNetworkAccess::Allow,
        )]));
        let resolved = vec!["10.0.0.10:80".parse().expect("socket addr")];

        let error = checked_socket_addrs("private.test", &policy, resolved)
            .expect_err("allowlisted domain resolving private should be blocked");

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn runtime_network_proxy_reports_resolved_private_target_blocks() {
        let proxy = RuntimeNetworkProxy::start(RuntimeNetworkPolicy::new(HashMap::from([(
            "blocked.example.com".to_string(),
            PermissionProfileNetworkAccess::Deny,
        )])))
        .expect("start proxy");
        let addr = proxy
            .proxy_url()
            .strip_prefix("http://")
            .expect("proxy url prefix")
            .to_string();
        let mut stream = TcpStream::connect(addr).expect("connect proxy");

        stream
            .write_all(b"GET http://localhost/ HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .expect("write request");
        let mut response = String::new();
        std::io::Read::read_to_string(&mut stream, &mut response).expect("read response");

        assert!(response.starts_with("HTTP/1.1 403 Forbidden"));
        assert!(
            response.contains("x-proxy-error: blocked-by-policy"),
            "response should identify resolved local target block: {response:?}"
        );
    }

    #[test]
    fn runtime_network_proxy_allows_loopback_targets_when_explicitly_allowlisted() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind local test server");
        let port = listener.local_addr().expect("server addr").port();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            let mut line = String::new();
            while reader.read_line(&mut line).expect("read request") != 0 {
                if line == "\r\n" || line == "\n" {
                    break;
                }
                line.clear();
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 7\r\n\r\nallowed")
                .expect("write response");
        });
        let proxy = RuntimeNetworkProxy::start(RuntimeNetworkPolicy::new(HashMap::from([(
            "127.0.0.1".to_string(),
            PermissionProfileNetworkAccess::Allow,
        )])))
        .expect("start proxy");
        let addr = proxy
            .proxy_url()
            .strip_prefix("http://")
            .expect("proxy url prefix")
            .to_string();
        let mut stream = TcpStream::connect(addr).expect("connect proxy");
        let request =
            format!("GET http://127.0.0.1:{port}/ HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n");

        stream.write_all(request.as_bytes()).expect("write request");
        let mut response = String::new();
        std::io::Read::read_to_string(&mut stream, &mut response).expect("read response");

        server.join().expect("server joined");
        assert!(
            response.contains("allowed"),
            "response should proxy explicitly allowlisted loopback target: {response:?}"
        );
    }
}
