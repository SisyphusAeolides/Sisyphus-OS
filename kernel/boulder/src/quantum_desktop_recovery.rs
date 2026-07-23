use crate::continuity_vault::{CheckpointId, ContinuityVault, VaultError};
use crate::quantum_crest_gateway::{
    CheckpointIntent, CheckpointReceipt, FocusIntent, ObjectQuery, ObjectReply, PresentIntent,
    PresentReceipt, QuantumCrestPlatform, RecoveryIntent, RecoveryReceipt,
};
use crate::sync::SpinLock;

use slope::quantum_crest::{
    STATUS_BACKEND, STATUS_DENIED, STATUS_INVALID, STATUS_OK, STATUS_STALE,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DesktopKernelState {
    pub epoch: u64,
    pub scene_epoch: u64,
    pub scene_root: u64,
    pub focus_surface: u64,
    pub focus_generation: u32,
    pub object_epoch: u32,
    pub present_sequence: u64,
    pub last_frame_root: u64,
    pub acknowledged_plan_root: u64,
    pub recovery_epoch: u64,
    pub safe_mode: bool,
}

impl DesktopKernelState {
    pub const fn new() -> Self {
        Self {
            epoch: 1,
            scene_epoch: 0,
            scene_root: 0,
            focus_surface: 0,
            focus_generation: 0,
            object_epoch: 1,
            present_sequence: 0,
            last_frame_root: 0,
            acknowledged_plan_root: 0,
            recovery_epoch: 0,
            safe_mode: false,
        }
    }

    pub fn root(self) -> u64 {
        let mut state = mix(0x4445_534b_544f_5021, self.epoch);
        state = mix(state, self.scene_epoch);
        state = mix(state, self.scene_root);
        state = mix(state, self.focus_surface);
        state = mix(state, u64::from(self.focus_generation));
        state = mix(state, u64::from(self.object_epoch));
        state = mix(state, self.present_sequence);
        state = mix(state, self.last_frame_root);
        state = mix(state, self.acknowledged_plan_root);
        state = mix(state, self.recovery_epoch);
        mix(state, u64::from(self.safe_mode))
    }
}

impl Default for DesktopKernelState {
    fn default() -> Self {
        Self::new()
    }
}

pub trait QuantumDisplayBroker: Sync {
    fn present(
        &self,
        framebuffer_object: u64,
        intent: PresentIntent,
    ) -> Result<PresentReceipt, i32>;

    fn release_surface(&self, surface: u64) -> Result<(), i32>;

    fn query_object(&self, query: ObjectQuery) -> Result<ObjectReply, i32>;
}

pub trait QuantumRecoveryBroker: Sync {
    fn recover(
        &self,
        intent: RecoveryIntent,
        checkpoint: Option<(CheckpointId, DesktopKernelState)>,
    ) -> Result<RecoveryReceipt, i32>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlatformStatistics {
    pub presents: u64,
    pub checkpoints: u64,
    pub recoveries: u64,
    pub focus_changes: u64,
    pub rejected: u64,
}

struct PlatformState {
    desktop: DesktopKernelState,
    latest_checkpoint: Option<CheckpointId>,
    statistics: PlatformStatistics,
}

impl PlatformState {
    const fn new() -> Self {
        Self {
            desktop: DesktopKernelState::new(),
            latest_checkpoint: None,
            statistics: PlatformStatistics {
                presents: 0,
                checkpoints: 0,
                recoveries: 0,
                focus_changes: 0,
                rejected: 0,
            },
        }
    }
}

pub struct QuantumBoulderPlatform<
    'a,
    Display: QuantumDisplayBroker + ?Sized,
    Recovery: QuantumRecoveryBroker + ?Sized,
    const CHECKPOINTS: usize,
> {
    display: &'a Display,
    recovery: &'a Recovery,
    vault: ContinuityVault<DesktopKernelState, CHECKPOINTS>,
    state: SpinLock<PlatformState>,
}

impl<
    'a,
    Display: QuantumDisplayBroker + ?Sized,
    Recovery: QuantumRecoveryBroker + ?Sized,
    const CHECKPOINTS: usize,
> QuantumBoulderPlatform<'a, Display, Recovery, CHECKPOINTS>
{
    pub const fn new(display: &'a Display, recovery: &'a Recovery) -> Self {
        Self {
            display,
            recovery,
            vault: ContinuityVault::new(),
            state: SpinLock::new(PlatformState::new()),
        }
    }

    pub fn state(&self) -> DesktopKernelState {
        self.state.lock().desktop
    }

    pub fn statistics(&self) -> PlatformStatistics {
        self.state.lock().statistics
    }

    pub fn latest_checkpoint(&self) -> Option<CheckpointId> {
        self.state.lock().latest_checkpoint
    }

    fn reject(&self) {
        let mut state = self.state.lock();
        state.statistics.rejected = state.statistics.rejected.saturating_add(1);
    }
}

impl<
    Display: QuantumDisplayBroker + ?Sized,
    Recovery: QuantumRecoveryBroker + ?Sized,
    const CHECKPOINTS: usize,
> QuantumCrestPlatform for QuantumBoulderPlatform<'_, Display, Recovery, CHECKPOINTS>
{
    fn present(&mut self, intent: PresentIntent) -> Result<PresentReceipt, i32> {
        if intent.framebuffer_object == 0
            || intent.certificate.frame_sequence == 0
            || intent.certificate.scene_root == 0
            || intent.certificate.root == 0
        {
            self.reject();
            return Err(STATUS_INVALID);
        }

        {
            let state = self.state.lock();
            if intent.certificate.frame_sequence <= state.desktop.present_sequence
                || (state.desktop.scene_epoch != 0
                    && intent.certificate.scene_epoch < state.desktop.scene_epoch)
            {
                drop(state);
                self.reject();
                return Err(STATUS_STALE);
            }
        }

        let receipt = self
            .display
            .present(intent.framebuffer_object, intent)
            .map_err(normalize_status)?;

        let mut state = self.state.lock();
        state.desktop.epoch = state.desktop.epoch.wrapping_add(1).max(1);
        state.desktop.scene_epoch = intent.certificate.scene_epoch;
        state.desktop.scene_root = intent.certificate.scene_root;
        state.desktop.present_sequence = intent.certificate.frame_sequence;
        state.desktop.last_frame_root = intent.certificate.root;
        state.statistics.presents = state.statistics.presents.saturating_add(1);
        Ok(receipt)
    }

    fn capture_checkpoint(&mut self, intent: CheckpointIntent) -> Result<CheckpointReceipt, i32> {
        if intent.scene_epoch == 0 || intent.scene_root == 0 {
            self.reject();
            return Err(STATUS_INVALID);
        }

        let desktop = {
            let mut state = self.state.lock();
            state.desktop.epoch = state.desktop.epoch.wrapping_add(1).max(1);
            state.desktop.scene_epoch = intent.scene_epoch;
            state.desktop.scene_root = intent.scene_root;
            state.desktop
        };

        let checkpoint = self
            .vault
            .checkpoint(&desktop, desktop.root(), intent.deadline_tick)
            .map_err(vault_status)?;

        let mut state = self.state.lock();
        state.latest_checkpoint = Some(checkpoint);
        state.statistics.checkpoints = state.statistics.checkpoints.saturating_add(1);

        Ok(CheckpointReceipt {
            checkpoint_handle: u64::from(checkpoint.slot) + 1,
            checkpoint_generation: checkpoint.generation,
            state_root: checkpoint.state_root,
        })
    }

    fn request_recovery(&mut self, intent: RecoveryIntent) -> Result<RecoveryReceipt, i32> {
        let checkpoint = {
            let state = self.state.lock();

            if intent.plan_root == 0 || intent.plan_root != state.desktop.acknowledged_plan_root {
                drop(state);
                self.reject();
                return Err(STATUS_DENIED);
            }

            match state.latest_checkpoint {
                Some(id) => {
                    let restored = self.vault.restore(id).map_err(vault_status)?;
                    Some((id, restored))
                }
                None => None,
            }
        };

        let receipt = self
            .recovery
            .recover(intent, checkpoint)
            .map_err(normalize_status)?;

        let mut state = self.state.lock();
        if let Some((_, restored)) = checkpoint {
            state.desktop = restored;
        }
        state.desktop.epoch = state.desktop.epoch.wrapping_add(1).max(1);
        state.desktop.recovery_epoch = receipt.recovery_epoch;
        state.desktop.safe_mode = intent.recovery_mode >= 2;
        state.statistics.recoveries = state.statistics.recoveries.saturating_add(1);
        Ok(receipt)
    }

    fn set_focus(&mut self, intent: FocusIntent) -> Result<u64, i32> {
        if intent.surface == 0 || intent.focus_generation == 0 {
            self.reject();
            return Err(STATUS_INVALID);
        }

        let mut state = self.state.lock();
        if intent.focus_generation < state.desktop.focus_generation {
            state.statistics.rejected = state.statistics.rejected.saturating_add(1);
            return Err(STATUS_STALE);
        }

        state.desktop.epoch = state.desktop.epoch.wrapping_add(1).max(1);
        state.desktop.focus_surface = intent.surface;
        state.desktop.focus_generation = intent.focus_generation;
        state.statistics.focus_changes = state.statistics.focus_changes.saturating_add(1);
        Ok(state.desktop.root())
    }

    fn release_surface(&mut self, surface: u64) -> Result<(), i32> {
        if surface == 0 {
            self.reject();
            return Err(STATUS_INVALID);
        }

        self.display
            .release_surface(surface)
            .map_err(normalize_status)?;

        let mut state = self.state.lock();
        if state.desktop.focus_surface == surface {
            state.desktop.focus_surface = 0;
            state.desktop.focus_generation = state.desktop.focus_generation.wrapping_add(1).max(1);
        }
        state.desktop.object_epoch = state.desktop.object_epoch.wrapping_add(1).max(1);
        state.desktop.epoch = state.desktop.epoch.wrapping_add(1).max(1);
        Ok(())
    }

    fn query_object(&mut self, query: ObjectQuery) -> Result<ObjectReply, i32> {
        if query.object == 0 {
            self.reject();
            return Err(STATUS_INVALID);
        }
        self.display.query_object(query).map_err(normalize_status)
    }

    fn acknowledge_plan(&mut self, plan_root: u64, _command_sequence: u64) -> Result<u64, i32> {
        if plan_root == 0 {
            self.reject();
            return Err(STATUS_INVALID);
        }

        let mut state = self.state.lock();
        state.desktop.epoch = state.desktop.epoch.wrapping_add(1).max(1);
        state.desktop.acknowledged_plan_root = plan_root;
        Ok(state.desktop.root())
    }
}

fn vault_status(error: VaultError) -> i32 {
    match error {
        VaultError::ZeroCapacity => STATUS_BACKEND,
        VaultError::InvalidCheckpoint | VaultError::StaleCheckpoint => STATUS_STALE,
    }
}

fn normalize_status(status: i32) -> i32 {
    match status {
        STATUS_OK | STATUS_INVALID | STATUS_DENIED | STATUS_STALE | STATUS_BACKEND => status,
        _ => STATUS_BACKEND,
    }
}

fn mix(mut state: u64, word: u64) -> u64 {
    state ^= word.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    state ^= state >> 30;
    state = state.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    state ^= state >> 27;
    state = state.wrapping_mul(0x94d0_49bb_1331_11eb);
    state ^ (state >> 31)
}
