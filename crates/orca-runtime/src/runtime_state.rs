use std::io;

use crate::extension::{ExtensionData, RuntimeExtensionStores};
use crate::runtime_capability::{RuntimeCapabilityPatch, RuntimeCapabilitySnapshot};
use crate::runtime_directive::{RuntimeDirective, RuntimeDirectiveState};
use crate::runtime_permission::{
    RuntimePermissionRequest, RuntimePermissionRequestHandler, RuntimePermissionResponse,
    TurnPermissionOverlay, TurnPermissionOverlayDelta,
};

#[derive(Clone, Copy, Debug)]
pub struct RuntimeTurnReducer {
    directive_state: DirectiveRuntimeState,
    permission_state: PermissionRuntimeState,
}

impl RuntimeTurnReducer {
    pub fn new(_thread_store: &ExtensionData, _turn_store: &ExtensionData) -> Self {
        Self {
            directive_state: DirectiveRuntimeState,
            permission_state: PermissionRuntimeState,
        }
    }

    pub fn from_extension_stores<'a>(extension_stores: RuntimeExtensionStores<'a>) -> Self {
        Self::new(
            extension_stores.thread_store(),
            extension_stores.turn_store(),
        )
    }

    pub fn apply_directive(
        &self,
        directive_state: &mut RuntimeDirectiveState,
        directive: RuntimeDirective,
    ) {
        self.directive_state.apply(directive_state, directive);
    }

    pub fn apply_capability_patch(
        &self,
        snapshot: &mut RuntimeCapabilitySnapshot,
        patch: RuntimeCapabilityPatch,
    ) {
        self.directive_state.apply_patch(snapshot, patch);
    }

    pub fn request_permission(
        &self,
        overlay: &mut TurnPermissionOverlay,
        handler: &dyn RuntimePermissionRequestHandler,
        request: RuntimePermissionRequest,
    ) -> io::Result<RuntimePermissionResponse> {
        self.permission_state
            .request_permission(overlay, handler, request)
    }

    pub fn merge_permission_overlay(
        &self,
        overlay: &mut TurnPermissionOverlay,
        other: &TurnPermissionOverlay,
    ) {
        self.permission_state
            .merge_permission_overlay(overlay, other);
    }

    pub fn merge_permission_delta(
        &self,
        overlay: &mut TurnPermissionOverlay,
        delta: &TurnPermissionOverlayDelta,
    ) {
        self.permission_state.merge_permission_delta(overlay, delta);
    }
}

#[derive(Clone, Copy, Debug)]
pub struct DirectiveRuntimeState;

impl DirectiveRuntimeState {
    pub fn apply(&self, directive_state: &mut RuntimeDirectiveState, directive: RuntimeDirective) {
        directive_state.apply(directive);
    }

    pub fn apply_patch(
        &self,
        snapshot: &mut RuntimeCapabilitySnapshot,
        patch: RuntimeCapabilityPatch,
    ) {
        snapshot.apply_patch(patch);
    }
}

#[derive(Clone, Copy, Debug)]
pub struct PermissionRuntimeState;

impl PermissionRuntimeState {
    pub fn request_permission(
        &self,
        overlay: &mut TurnPermissionOverlay,
        handler: &dyn RuntimePermissionRequestHandler,
        request: RuntimePermissionRequest,
    ) -> io::Result<RuntimePermissionResponse> {
        overlay.request_and_merge(handler, request)
    }

    pub fn merge_permission_overlay(
        &self,
        overlay: &mut TurnPermissionOverlay,
        other: &TurnPermissionOverlay,
    ) {
        overlay.merge(other);
    }

    pub fn merge_permission_delta(
        &self,
        overlay: &mut TurnPermissionOverlay,
        delta: &TurnPermissionOverlayDelta,
    ) {
        overlay.apply_delta(delta);
    }
}
