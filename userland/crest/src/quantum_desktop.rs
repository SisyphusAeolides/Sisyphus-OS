use slope::quantum_crest::{
    QuantumCrestPage, QuantumDesktopReply, QuantumSystemSnapshot, SNAPSHOT_FLAG_SAFE_MODE,
    STATUS_OK,
};

use crate::compositor::pipeline::{CompositorError, CompositorPipeline};
use crate::input::{InputEvent, IntegratedEvent};
use crate::manifold::DisplayMode;
use crate::obsidian::ObsidianShell;
use crate::quantum_aura::{AlienPalette, QuantumAura};
use crate::quantum_focus::{
    FocusClass, FocusError, FocusGrant, InputDispatch, QuantumFocusLattice,
};
use crate::quantum_frame_oracle::{
    FrameObservation, FrameOracleError, QuantumFrameOracle, QuantumFramePlan,
};
use crate::quantum_portal::{PortalClientError, PresentCommand, QuantumPortalClient};
use crate::quantum_scene::{
    QuantumScene, SceneCommit, SceneError, SceneLease, SceneNode, SceneTransaction,
};
use crate::quantum_tile_field::{QuantumDamageError, QuantumTileField};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QuantumDesktopSecrets {
    pub scene: u64,
    pub focus: u64,
    pub damage: u64,
    pub frame: u64,
    pub snapshot: u64,
    pub command: u64,
}

impl QuantumDesktopSecrets {
    pub const fn valid(self) -> bool {
        let values = [
            self.scene,
            self.focus,
            self.damage,
            self.frame,
            self.snapshot,
            self.command,
        ];

        let mut i = 0;
        while i < values.len() {
            if values[i] == 0 {
                return false;
            }
            let mut j = i + 1;
            while j < values.len() {
                if values[i] == values[j] {
                    return false;
                }
                j += 1;
            }
            i += 1;
        }

        true
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DesktopError {
    InvalidSecrets,
    SnapshotModeMismatch,
    NoSnapshot,
    Scene(SceneError),
    Focus(FocusError),
    Damage(QuantumDamageError),
    Oracle(FrameOracleError),
    Portal(PortalClientError),
    Compositor(CompositorError),
    BufferTooSmall,
    FrameAlreadyPending,
    NoPendingFrame,
    ReplyFailed(i32),
}

impl From<SceneError> for DesktopError {
    fn from(error: SceneError) -> Self {
        Self::Scene(error)
    }
}

impl From<FocusError> for DesktopError {
    fn from(error: FocusError) -> Self {
        Self::Focus(error)
    }
}

impl From<QuantumDamageError> for DesktopError {
    fn from(error: QuantumDamageError) -> Self {
        Self::Damage(error)
    }
}

impl From<FrameOracleError> for DesktopError {
    fn from(error: FrameOracleError) -> Self {
        Self::Oracle(error)
    }
}

impl From<PortalClientError> for DesktopError {
    fn from(error: PortalClientError) -> Self {
        Self::Portal(error)
    }
}

impl From<CompositorError> for DesktopError {
    fn from(error: CompositorError) -> Self {
        Self::Compositor(error)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DesktopFrameSubmission {
    pub command_sequence: u64,
    pub frame_sequence: u64,
    pub snapshot_sequence: u64,
    pub scene_epoch: u64,
    pub scene_root: u64,
    pub damage_root: u64,
    pub rendered_tiles: u32,
    pub skipped_tiles: u32,
    pub render_ticks: u64,
    pub deadline_tick: u64,
    pub lane_votes: u8,
    pub confidence: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DesktopFrameResult {
    Idle {
        snapshot_sequence: u64,
        scene_root: u64,
    },
    Submitted(DesktopFrameSubmission),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DesktopReply {
    pub reply: QuantumDesktopReply,
    pub frame: Option<DesktopFrameSubmission>,
}

#[derive(Clone, Copy)]
struct PendingFrame {
    submission: DesktopFrameSubmission,
    plan: QuantumFramePlan,
    render_begin_tick: u64,
    render_end_tick: u64,
}

pub struct QuantumDesktop<'a, const SCENE: usize, const MUTATIONS: usize, const FOCUS: usize> {
    mode: DisplayMode,
    scene: QuantumScene<SCENE>,
    focus: QuantumFocusLattice<FOCUS>,
    tiles: QuantumTileField,
    oracle: QuantumFrameOracle,
    aura: QuantumAura,
    pipeline: CompositorPipeline,
    portal: QuantumPortalClient<'a>,
    damage_secret: u64,
    snapshot: Option<QuantumSystemSnapshot>,
    pending_frame: Option<PendingFrame>,
}

impl<'a, const SCENE: usize, const MUTATIONS: usize, const FOCUS: usize>
    QuantumDesktop<'a, SCENE, MUTATIONS, FOCUS>
{
    pub fn new(
        mode: DisplayMode,
        page: &'a QuantumCrestPage,
        secrets: QuantumDesktopSecrets,
    ) -> Result<Self, DesktopError> {
        if !secrets.valid() {
            return Err(DesktopError::InvalidSecrets);
        }

        let mut pipeline = CompositorPipeline::new(mode);
        pipeline.damage.clear_all();

        Ok(Self {
            mode,
            scene: QuantumScene::new(secrets.scene)?,
            focus: QuantumFocusLattice::new(secrets.focus)?,
            tiles: QuantumTileField::new(mode)?,
            oracle: QuantumFrameOracle::new(secrets.frame)?,
            aura: QuantumAura::new(AlienPalette::OBSIDIAN_CIVILIZATION),
            pipeline,
            portal: QuantumPortalClient::new(page, secrets.snapshot, secrets.command)?,
            damage_secret: secrets.damage,
            snapshot: None,
            pending_frame: None,
        })
    }

    pub const fn mode(&self) -> DisplayMode {
        self.mode
    }

    pub const fn snapshot(&self) -> Option<QuantumSystemSnapshot> {
        self.snapshot
    }

    pub const fn scene(&self) -> &QuantumScene<SCENE> {
        &self.scene
    }

    pub fn scene_mut(&mut self) -> &mut QuantumScene<SCENE> {
        &mut self.scene
    }

    pub const fn focus(&self) -> &QuantumFocusLattice<FOCUS> {
        &self.focus
    }

    pub fn synchronize(&mut self) -> Result<QuantumSystemSnapshot, DesktopError> {
        let snapshot = self.portal.synchronize()?;

        if snapshot.display.width != 0
            && (snapshot.display.width != self.mode.width
                || snapshot.display.height != self.mode.height
                || snapshot.display.pitch != self.mode.pitch)
        {
            return Err(DesktopError::SnapshotModeMismatch);
        }

        if self
            .snapshot
            .is_none_or(|previous| previous.sequence != snapshot.sequence)
        {
            self.tiles
                .escalate_blacklab(snapshot.blacklab.risk, snapshot.logical_tick);
        }

        self.aura
            .synchronize(snapshot, self.scene.root(), self.focus.root());
        self.snapshot = Some(snapshot);
        Ok(snapshot)
    }

    pub fn spawn_surface(
        &mut self,
        node: SceneNode,
        rights: u32,
        expires_epoch: u64,
        tick: u64,
    ) -> Result<SceneLease, DesktopError> {
        let rectangle = node.rectangle;
        let lease = self.scene.spawn(node, rights, expires_epoch)?;
        self.tiles.mark_rectangle(rectangle, tick, 1 << 1, false);
        Ok(lease)
    }

    pub fn commit_scene(
        &mut self,
        transaction: SceneTransaction<MUTATIONS>,
        leases: &[SceneLease],
        tick: u64,
    ) -> Result<SceneCommit<MUTATIONS>, DesktopError> {
        let commit = self.scene.commit(transaction, leases)?;
        self.tiles.absorb_scene_commit(commit.dirty(), tick, 1 << 2);
        self.aura.synchronize(
            self.snapshot.ok_or(DesktopError::NoSnapshot)?,
            commit.root,
            self.focus.root(),
        );
        Ok(commit)
    }

    pub fn grant_focus(
        &mut self,
        lease: SceneLease,
        rights: u32,
        expires_tick: u64,
        now_tick: u64,
    ) -> Result<FocusGrant, DesktopError> {
        Ok(self
            .focus
            .grant(&self.scene, lease, rights, expires_tick, now_tick)?)
    }

    pub fn activate_focus(
        &mut self,
        grant: FocusGrant,
        class: FocusClass,
        now_tick: u64,
    ) -> Result<u64, DesktopError> {
        let root = self.focus.activate(grant, class, now_tick)?;
        self.aura.synchronize(
            self.snapshot.ok_or(DesktopError::NoSnapshot)?,
            self.scene.root(),
            root,
        );
        Ok(root)
    }

    pub fn dispatch_input(
        &mut self,
        observed: IntegratedEvent,
        predicted: Option<InputEvent>,
        now_tick: u64,
    ) -> Result<InputDispatch, DesktopError> {
        self.tiles.predict_input(observed, predicted, now_tick);
        let dispatch = self
            .focus
            .route(&self.scene, observed, predicted, now_tick)?;
        Ok(dispatch)
    }

    pub fn render_and_submit(
        &mut self,
        shell: &ObsidianShell,
        buffer: &mut [u8],
        framebuffer_object: u64,
        beam_position: u32,
        present_flags: u64,
        mut clock: impl FnMut() -> u64,
    ) -> Result<DesktopFrameResult, DesktopError> {
        if self.pending_frame.is_some() || self.portal.session()?.pending_sequence.is_some() {
            return Err(DesktopError::FrameAlreadyPending);
        }
        if buffer.len() < self.mode.required_bytes() as usize {
            return Err(DesktopError::BufferTooSmall);
        }

        let snapshot = self.synchronize()?;
        let render_begin_tick = clock();

        let mut schedule = self.tiles.compile_schedule(
            self.tiles.total_tiles(),
            beam_position,
            self.damage_secret,
        )?;

        if schedule.scheduled == 0 {
            return Ok(DesktopFrameResult::Idle {
                snapshot_sequence: snapshot.sequence,
                scene_root: self.scene.root(),
            });
        }

        let mut plan = self.oracle.plan(snapshot, &schedule, render_begin_tick)?;

        if plan.tile_budget < schedule.scheduled {
            schedule = self
                .tiles
                .compile_schedule(plan.tile_budget, beam_position, self.damage_secret)?;
            plan = self.oracle.bind_schedule(plan, &schedule)?;
        }

        self.pipeline.damage.clear_all();
        self.tiles.apply_schedule(&schedule, &mut self.pipeline);

        let rendered = self
            .pipeline
            .render_schedule(shell, buffer, schedule.indices())?;
        self.aura
            .synchronize(snapshot, self.scene.root(), self.focus.root());
        self.aura.apply_schedule(buffer, self.mode, &schedule);

        let render_end_tick = clock();
        let render_ticks = render_end_tick.saturating_sub(render_begin_tick);
        let skipped = schedule
            .scheduled
            .saturating_sub(rendered as usize)
            .min(u32::MAX as usize) as u32;

        self.tiles.complete_frame(&schedule, render_end_tick);

        let command_sequence = match self.portal.submit_present(PresentCommand {
            framebuffer_object,
            frame_sequence: plan.frame_sequence,
            snapshot_sequence: snapshot.sequence,
            scene_epoch: self.scene.epoch(),
            damage_root: schedule.root,
            scene_root: self.scene.root(),
            rendered_tiles: rendered,
            skipped_tiles: skipped,
            lane_votes: plan.lane_votes,
            deadline_tick: plan.deadline_tick,
            flags: present_flags,
        }) {
            Ok(sequence) => sequence,
            Err(error) => {
                self.tiles.escalate_blacklab(1000, render_end_tick);
                return Err(DesktopError::Portal(error));
            }
        };

        let submission = DesktopFrameSubmission {
            command_sequence,
            frame_sequence: plan.frame_sequence,
            snapshot_sequence: snapshot.sequence,
            scene_epoch: self.scene.epoch(),
            scene_root: self.scene.root(),
            damage_root: schedule.root,
            rendered_tiles: rendered,
            skipped_tiles: skipped,
            render_ticks,
            deadline_tick: plan.deadline_tick,
            lane_votes: plan.lane_votes,
            confidence: plan.confidence,
        };
        self.pending_frame = Some(PendingFrame {
            submission,
            plan,
            render_begin_tick,
            render_end_tick,
        });

        Ok(DesktopFrameResult::Submitted(submission))
    }

    pub fn poll_reply(
        &mut self,
        mut clock: impl FnMut() -> u64,
    ) -> Result<Option<DesktopReply>, DesktopError> {
        let Some(reply) = self.portal.take_reply()? else {
            return Ok(None);
        };

        let pending = self.pending_frame.take();
        if reply.status != STATUS_OK {
            let tick = clock();
            self.tiles.escalate_blacklab(1000, tick);
            return Err(DesktopError::ReplyFailed(reply.status));
        }

        let completed = if let Some(pending) = pending {
            let present_tick = if reply.outputs[1] != 0 {
                reply.outputs[1]
            } else {
                clock()
            };
            self.oracle.observe(FrameObservation {
                frame_sequence: pending.submission.frame_sequence,
                rendered_tiles: pending.submission.rendered_tiles.max(1) as usize,
                render_ticks: pending
                    .render_end_tick
                    .saturating_sub(pending.render_begin_tick),
                missed_deadline: present_tick >= pending.submission.deadline_tick,
                present_tick,
            })?;
            Some(pending.submission)
        } else {
            None
        };

        Ok(Some(DesktopReply {
            reply,
            frame: completed,
        }))
    }

    pub fn request_checkpoint(
        &mut self,
        object: u64,
        deadline_tick: u64,
    ) -> Result<u64, DesktopError> {
        Ok(self.portal.capture_checkpoint(
            object,
            self.scene.epoch(),
            self.scene.root(),
            deadline_tick,
        )?)
    }

    pub fn request_recovery(
        &mut self,
        object: u64,
        recovery_mode: u32,
        deadline_tick: u64,
    ) -> Result<u64, DesktopError> {
        let snapshot = self.snapshot.ok_or(DesktopError::NoSnapshot)?;
        Ok(self.portal.request_recovery(
            object,
            snapshot.blacklab.plan_root,
            recovery_mode,
            deadline_tick,
        )?)
    }

    pub fn acknowledge_blacklab_plan(&mut self, deadline_tick: u64) -> Result<u64, DesktopError> {
        let snapshot = self.snapshot.ok_or(DesktopError::NoSnapshot)?;
        Ok(self
            .portal
            .acknowledge_plan(snapshot.blacklab.plan_root, deadline_tick)?)
    }

    pub fn enter_safe_mode(&mut self, tick: u64) -> Result<(), DesktopError> {
        let mut snapshot = self.snapshot.ok_or(DesktopError::NoSnapshot)?;
        snapshot.flags |= SNAPSHOT_FLAG_SAFE_MODE;
        self.snapshot = Some(snapshot);
        self.tiles.escalate_blacklab(1000, tick);
        self.aura
            .synchronize(snapshot, self.scene.root(), self.focus.root());
        Ok(())
    }

    pub fn frame_oracle(&self) -> &QuantumFrameOracle {
        &self.oracle
    }

    pub fn aura(&self) -> &QuantumAura {
        &self.aura
    }

    pub fn latest_plan(&self) -> Option<QuantumFramePlan> {
        self.pending_frame.map(|pending| pending.plan)
    }
}
