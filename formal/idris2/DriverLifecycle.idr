module DriverLifecycle

import Decidable.Equality
import Data.Nat

%hide Data.Nat.NonZero

%default total

public export
data DriverPhase
  = Discovered
  | Admitted
  | Online
  | Quarantined
  | Released

public export
data IsTrue : Bool -> Type where
  Proven : IsTrue True

public export
data NonZero : Nat -> Type where
  IsSuccessor : NonZero (S value)

public export
record RawAdmission where
  constructor MkRawAdmission
  routedRequester : Nat
  iommuDomain : Nat
  translationEnabled : Bool
  firmwareMeasured : Bool
  mmioScoped : Bool
  irqScoped : Bool

public export
record AdmissionCertificate (requester : Nat) where
  constructor MkAdmissionCertificate
  routedRequester : Nat
  iommuDomain : Nat
  requesterAgreement : requester = routedRequester
  liveDomain : NonZero iommuDomain
  translationProof : IsTrue True
  firmwareProof : IsTrue True
  mmioProof : IsTrue True
  irqProof : IsTrue True

public export
data AdmissionFault
  = RequesterMismatch
  | MissingDomain
  | TranslationDisabled
  | FirmwareUnmeasured
  | MmioUnscoped
  | IrqUnscoped

public export
data AdmissionDecision : Nat -> Type where
  AdmissionRejected : AdmissionFault -> AdmissionDecision requester
  AdmissionAccepted : AdmissionCertificate requester -> AdmissionDecision requester

verifyMatched :
  (requester : Nat) ->
  (domain : Nat) ->
  (translation : Bool) ->
  (firmware : Bool) ->
  (mmio : Bool) ->
  (irq : Bool) ->
  AdmissionDecision requester
verifyMatched requester Z translation firmware mmio irq =
  AdmissionRejected MissingDomain
verifyMatched requester (S domain) False firmware mmio irq =
  AdmissionRejected TranslationDisabled
verifyMatched requester (S domain) True False mmio irq =
  AdmissionRejected FirmwareUnmeasured
verifyMatched requester (S domain) True True False irq =
  AdmissionRejected MmioUnscoped
verifyMatched requester (S domain) True True True False =
  AdmissionRejected IrqUnscoped
verifyMatched requester (S domain) True True True True =
  AdmissionAccepted
    (MkAdmissionCertificate
      requester
      (S domain)
      Refl
      IsSuccessor
      Proven
      Proven
      Proven
      Proven)

public export
verifyAdmission : (requester : Nat) -> RawAdmission -> AdmissionDecision requester
verifyAdmission requester
  (MkRawAdmission routed domain translation firmware mmio irq) =
    case decEq requester routed of
      No mismatch => AdmissionRejected RequesterMismatch
      Yes Refl => verifyMatched requester domain translation firmware mmio irq

public export
data Driver : DriverPhase -> Nat -> Type where
  Seen : Driver Discovered requester
  Bound : AdmissionCertificate requester -> Driver Admitted requester
  Serving : AdmissionCertificate requester -> Driver Online requester
  Contained : AdmissionCertificate requester -> Driver Quarantined requester
  Gone : Driver Released requester

public export
data DriverTransition : DriverPhase -> DriverPhase -> Nat -> Type where
  Admit : AdmissionCertificate requester ->
          DriverTransition Discovered Admitted requester
  Start : DriverTransition Admitted Online requester
  DetectFault : DriverTransition Online Quarantined requester
  Stop : DriverTransition Online Released requester
  Revoke : DriverTransition Quarantined Released requester

public export
advanceDriver :
  Driver before requester ->
  DriverTransition before after requester ->
  Driver after requester
advanceDriver Seen (Admit certificate) = Bound certificate
advanceDriver (Bound certificate) Start = Serving certificate
advanceDriver (Serving certificate) DetectFault = Contained certificate
advanceDriver (Serving certificate) Stop = Gone
advanceDriver (Contained certificate) Revoke = Gone

public export
admitObserved :
  {requester : Nat} ->
  Driver Discovered requester ->
  RawAdmission ->
  Either AdmissionFault (Driver Admitted requester)
admitObserved {requester} Seen observation =
  case verifyAdmission requester observation of
    AdmissionRejected fault => Left fault
    AdmissionAccepted certificate => Right (advanceDriver Seen (Admit certificate))

public export
sampleAdmission : AdmissionDecision 42
sampleAdmission =
  verifyAdmission 42 (MkRawAdmission 42 7 True True True True)

public export
sampleAdmissionAccepted : Bool
sampleAdmissionAccepted =
  case sampleAdmission of
    AdmissionRejected fault => False
    AdmissionAccepted certificate => True

-- A binding reservation is indexed by both the observed device and its ledger
-- generation. There is deliberately no transition from rollback debt to a new
-- candidate: only a complete cleanup advances the generation and returns the
-- slot to DetectedBinding.

public export
data BindingPhase
  = DetectedBinding
  | ReservedBinding
  | ActiveBinding
  | RollbackDebt
  | BindingQuarantined
  | DeferredBinding

public export
record RawMatch where
  constructor MkRawMatch
  observedDevice : Nat
  selectedDriver : Nat
  identityMeasured : Bool
  observedClassTuple : Nat
  selectedClassTuple : Nat

public export
record MatchCertificate (device : Nat) where
  constructor MkMatchCertificate
  observedDevice : Nat
  selectedDriver : Nat
  observedClassTuple : Nat
  selectedClassTuple : Nat
  deviceAgreement : device = observedDevice
  classAgreement : observedClassTuple = selectedClassTuple
  liveDriver : NonZero selectedDriver
  identityProof : IsTrue True

public export
data MatchFault
  = DeviceMismatch
  | MissingDriver
  | IdentityUnmeasured
  | ClassTupleMismatch

public export
data MatchDecision : Nat -> Type where
  MatchRejected : MatchFault -> MatchDecision device
  MatchAccepted : MatchCertificate device -> MatchDecision device

verifyExactMatch :
  (device : Nat) ->
  (driver : Nat) ->
  (identity : Bool) ->
  (observedClass : Nat) ->
  (selectedClass : Nat) ->
  MatchDecision device
verifyExactMatch device Z identity observedClass selectedClass =
  MatchRejected MissingDriver
verifyExactMatch device (S driver) False observedClass selectedClass =
  MatchRejected IdentityUnmeasured
verifyExactMatch device (S driver) True observedClass selectedClass =
  case decEq observedClass selectedClass of
    No mismatch => MatchRejected ClassTupleMismatch
    Yes Refl =>
      MatchAccepted
        (MkMatchCertificate
          device
          (S driver)
          selectedClass
          selectedClass
          Refl
          Refl
          IsSuccessor
          Proven)

public export
verifyMatch : (device : Nat) -> RawMatch -> MatchDecision device
verifyMatch device
  (MkRawMatch observed driver identity observedClass selectedClass) =
  case decEq device observed of
    No mismatch => MatchRejected DeviceMismatch
    Yes Refl =>
      verifyExactMatch device driver identity observedClass selectedClass

public export
data DeviceBinding : BindingPhase -> Nat -> Nat -> Type where
  DetectedDevice : DeviceBinding DetectedBinding device generation
  ReservedDevice :
    MatchCertificate device ->
    DeviceBinding ReservedBinding device generation
  ServingDevice :
    MatchCertificate device ->
    DeviceBinding ActiveBinding device generation
  CleanupOwed :
    MatchCertificate device ->
    DeviceBinding RollbackDebt device generation
  ContainedDevice :
    MatchCertificate device ->
    DeviceBinding BindingQuarantined device generation
  DeferredDevice :
    MatchCertificate device ->
    DeviceBinding DeferredBinding device generation

public export
data BindingTransition :
  BindingPhase -> Nat -> BindingPhase -> Nat -> Nat -> Type where
  ReserveDevice :
    MatchCertificate device ->
    BindingTransition
      DetectedBinding generation ReservedBinding generation device
  ActivateDevice :
    BindingTransition
      ReservedBinding generation ActiveBinding generation device
  BeginCleanup :
    BindingTransition
      ReservedBinding generation RollbackDebt generation device
  DetectBindingFault :
    BindingTransition
      ActiveBinding generation RollbackDebt generation device
  FinishExactCleanup :
    BindingTransition
      RollbackDebt generation DetectedBinding (S generation) device
  QuarantineCleanupDebt :
    BindingTransition
      RollbackDebt generation BindingQuarantined generation device
  DeferToEnumerator :
    BindingTransition
      ReservedBinding generation DeferredBinding generation device

public export
advanceBinding :
  DeviceBinding before device generationBefore ->
  BindingTransition before generationBefore after generationAfter device ->
  DeviceBinding after device generationAfter
advanceBinding DetectedDevice (ReserveDevice certificate) =
  ReservedDevice certificate
advanceBinding (ReservedDevice certificate) ActivateDevice =
  ServingDevice certificate
advanceBinding (ReservedDevice certificate) BeginCleanup =
  CleanupOwed certificate
advanceBinding (ServingDevice certificate) DetectBindingFault =
  CleanupOwed certificate
advanceBinding (CleanupOwed certificate) FinishExactCleanup = DetectedDevice
advanceBinding (CleanupOwed certificate) QuarantineCleanupDebt =
  ContainedDevice certificate
advanceBinding (ReservedDevice certificate) DeferToEnumerator =
  DeferredDevice certificate

public export
data CleanupResult = CleanupComplete | CleanupPending

public export
data MayTryNext : CleanupResult -> Type where
  ExactRollback : MayTryNext CleanupComplete

public export
fallbackDecision : CleanupResult -> Bool
fallbackDecision CleanupComplete = True
fallbackDecision CleanupPending = False

public export
sampleBinding : MatchDecision 42
sampleBinding = verifyMatch 42 (MkRawMatch 42 9 True 787248 787248)

-- xHCI initialization is a separate, geometry-indexed protocol.  Read-only
-- capability observation establishes the offsets which the later BAR
-- measurement must bound.  Once firmware ownership or controller state has
-- been changed, failures become mutation debt and can only be contained.

public export
data XhciPhase
  = XhciClaimed
  | XhciCapabilityProvisional
  | XhciFirmwareResolved
  | XhciHalted
  | XhciApertureMeasured
  | XhciResetReady
  | XhciProtocolMapped
  | XhciRingsReady
  | XhciOperationalDeferred
  | XhciMutationDebt
  | XhciQuarantined

public export
record RawXhciAuthorization where
  constructor MkRawXhciAuthorization
  authorizedDevice : Nat
  authorizedGeneration : Nat
  censusRoot : Nat
  authorizationRoot : Nat
  authorizationLive : Bool

public export
record LiveXhciAuthorization (device : Nat) (generation : Nat) where
  constructor MkLiveXhciAuthorization
  authorizedDevice : Nat
  authorizedGeneration : Nat
  censusRoot : Nat
  authorizationRoot : Nat
  deviceAgreement : device = authorizedDevice
  generationAgreement : generation = authorizedGeneration
  liveCensusRoot : NonZero censusRoot
  liveAuthorizationRoot : NonZero authorizationRoot
  livenessProof : IsTrue True

public export
data XhciAuthorizationFault
  = XhciDeviceMismatch
  | XhciGenerationMismatch
  | XhciCensusRootMissing
  | XhciAuthorizationRootMissing
  | XhciAuthorizationExpired

public export
data XhciAuthorizationDecision : Nat -> Nat -> Type where
  XhciAuthorizationRejected :
    XhciAuthorizationFault ->
    XhciAuthorizationDecision device generation
  XhciAuthorizationAccepted :
    LiveXhciAuthorization device generation ->
    XhciAuthorizationDecision device generation

verifyLiveXhciRoots :
  (device : Nat) ->
  (generation : Nat) ->
  (censusRoot : Nat) ->
  (authorizationRoot : Nat) ->
  (live : Bool) ->
  XhciAuthorizationDecision device generation
verifyLiveXhciRoots device generation Z authorizationRoot live =
  XhciAuthorizationRejected XhciCensusRootMissing
verifyLiveXhciRoots device generation (S censusRoot) Z live =
  XhciAuthorizationRejected XhciAuthorizationRootMissing
verifyLiveXhciRoots device generation (S censusRoot) (S authorizationRoot) False =
  XhciAuthorizationRejected XhciAuthorizationExpired
verifyLiveXhciRoots device generation (S censusRoot) (S authorizationRoot) True =
  XhciAuthorizationAccepted
    (MkLiveXhciAuthorization
      device
      generation
      (S censusRoot)
      (S authorizationRoot)
      Refl
      Refl
      IsSuccessor
      IsSuccessor
      Proven)

public export
verifyLiveXhciAuthorization :
  (device : Nat) ->
  (generation : Nat) ->
  RawXhciAuthorization ->
  XhciAuthorizationDecision device generation
verifyLiveXhciAuthorization device generation
  (MkRawXhciAuthorization authorizedDevice authorizedGeneration
    censusRoot authorizationRoot live) =
    case decEq device authorizedDevice of
      No mismatch => XhciAuthorizationRejected XhciDeviceMismatch
      Yes Refl =>
        case decEq generation authorizedGeneration of
          No mismatch => XhciAuthorizationRejected XhciGenerationMismatch
          Yes Refl =>
            verifyLiveXhciRoots
              device generation censusRoot authorizationRoot live

public export
record XhciGeometry where
  constructor MkXhciGeometry
  capabilityEnd : Nat
  operationalEnd : Nat
  runtimeEnd : Nat
  doorbellEnd : Nat

public export
record ProvisionalCapabilityReceipt
  (device : Nat)
  (generation : Nat)
  (geometry : XhciGeometry) where
    constructor MkProvisionalCapabilityReceipt
    capabilityRoot : Nat
    liveCapabilityRoot : NonZero capabilityRoot
    capabilityWindowPresent : NonZero (capabilityEnd geometry)
    operationalWindowPresent : NonZero (operationalEnd geometry)
    runtimeWindowPresent : NonZero (runtimeEnd geometry)
    doorbellWindowPresent : NonZero (doorbellEnd geometry)

public export
data FirmwareResolutionReceipt : Nat -> Nat -> Type where
  FirmwareOwnershipReceipt :
    (legacyOffset : Nat) ->
    NonZero legacyOffset ->
    (ownershipRoot : Nat) ->
    NonZero ownershipRoot ->
    FirmwareResolutionReceipt device generation
  NoLegacyCapabilityReceipt :
    (capabilityChainRoot : Nat) ->
    NonZero capabilityChainRoot ->
    FirmwareResolutionReceipt device generation

public export
record HaltReceipt (device : Nat) (generation : Nat) where
  constructor MkHaltReceipt
  haltRoot : Nat
  liveHaltRoot : NonZero haltRoot
  haltedProof : IsTrue True

public export
record MeasuredApertureReceipt
  (device : Nat)
  (generation : Nat)
  (geometry : XhciGeometry) where
    constructor MkMeasuredApertureReceipt
    apertureBase : Nat
    apertureBytes : Nat
    measurementRoot : Nat
    liveApertureBase : NonZero apertureBase
    liveAperture : NonZero apertureBytes
    liveMeasurementRoot : NonZero measurementRoot
    capabilityWithinAperture : LTE (capabilityEnd geometry) apertureBytes
    operationalWithinAperture : LTE (operationalEnd geometry) apertureBytes
    runtimeWithinAperture : LTE (runtimeEnd geometry) apertureBytes
    doorbellWithinAperture : LTE (doorbellEnd geometry) apertureBytes

public export
record ResetReadyReceipt (device : Nat) (generation : Nat) where
  constructor MkResetReadyReceipt
  resetRoot : Nat
  liveResetRoot : NonZero resetRoot
  controllerNotReadyCleared : IsTrue True
  controllerRemainsHalted : IsTrue True

public export
record ProtocolMapReceipt (device : Nat) (generation : Nat) where
  constructor MkProtocolMapReceipt
  protocolCount : Nat
  protocolRoot : Nat
  liveProtocolCount : NonZero protocolCount
  liveProtocolRoot : NonZero protocolRoot
  portOneHasProtocol : IsTrue True

public export
record RingReadyReceipt (device : Nat) (generation : Nat) where
  constructor MkRingReadyReceipt
  commandCapacity : Nat
  eventCapacity : Nat
  ringRoot : Nat
  liveCommandCapacity : NonZero commandCapacity
  liveEventCapacity : NonZero eventCapacity
  liveRingRoot : NonZero ringRoot
  commandCyclePublicationBound : IsTrue True
  completionCorrelationBound : IsTrue True

public export
data XhciOperationalPrerequisite
  = DcbaaAllocationRequired
  | CommandRingRequired
  | EventRingRequired
  | InterruptRouteRequired
  | ProtocolPortRoutingRequired

public export
record DeferredOperationalReceipt (device : Nat) (generation : Nat) where
  constructor MkDeferredOperationalReceipt
  missingPrerequisite : XhciOperationalPrerequisite
  deferralRoot : Nat
  liveDeferralRoot : NonZero deferralRoot

public export
data XhciMutationFault : XhciPhase -> Type where
  FirmwareOwnershipTimedOut :
    XhciMutationFault XhciCapabilityProvisional
  HaltTimedOut :
    XhciMutationFault XhciFirmwareResolved
  ApertureMeasurementFailed :
    XhciMutationFault XhciHalted
  ControllerResetTimedOut :
    XhciMutationFault XhciApertureMeasured
  ProtocolMapRejected :
    XhciMutationFault XhciResetReady
  RingInitializationRejected :
    XhciMutationFault XhciProtocolMapped

public export
record XhciDebtReceipt
  (device : Nat)
  (generation : Nat)
  (failedPhase : XhciPhase) where
    constructor MkXhciDebtReceipt
    debtRoot : Nat
    liveDebtRoot : NonZero debtRoot
    debtFault : XhciMutationFault failedPhase

public export
record XhciQuarantineReceipt (device : Nat) (generation : Nat) where
  constructor MkXhciQuarantineReceipt
  quarantineRoot : Nat
  liveQuarantineRoot : NonZero quarantineRoot

public export
data XhciController :
  XhciPhase -> Nat -> Nat -> Maybe XhciGeometry -> Type where
  ClaimedXhci :
    MatchCertificate device ->
    LiveXhciAuthorization device generation ->
    XhciController XhciClaimed device generation Nothing
  ProvisionalXhci :
    XhciController XhciClaimed device generation Nothing ->
    ProvisionalCapabilityReceipt device generation geometry ->
    XhciController
      XhciCapabilityProvisional device generation (Just geometry)
  FirmwareResolvedXhci :
    XhciController
      XhciCapabilityProvisional device generation (Just geometry) ->
    FirmwareResolutionReceipt device generation ->
    XhciController XhciFirmwareResolved device generation (Just geometry)
  HaltedXhci :
    XhciController XhciFirmwareResolved device generation (Just geometry) ->
    HaltReceipt device generation ->
    XhciController XhciHalted device generation (Just geometry)
  ApertureMeasuredXhci :
    XhciController XhciHalted device generation (Just geometry) ->
    MeasuredApertureReceipt device generation geometry ->
    XhciController XhciApertureMeasured device generation (Just geometry)
  ResetReadyXhci :
    XhciController XhciApertureMeasured device generation (Just geometry) ->
    ResetReadyReceipt device generation ->
    XhciController XhciResetReady device generation (Just geometry)
  ProtocolMappedXhci :
    XhciController XhciResetReady device generation (Just geometry) ->
    ProtocolMapReceipt device generation ->
    XhciController XhciProtocolMapped device generation (Just geometry)
  RingsReadyXhci :
    XhciController XhciProtocolMapped device generation (Just geometry) ->
    RingReadyReceipt device generation ->
    XhciController XhciRingsReady device generation (Just geometry)
  OperationalDeferredXhci :
    XhciController XhciRingsReady device generation (Just geometry) ->
    DeferredOperationalReceipt device generation ->
    XhciController XhciOperationalDeferred device generation (Just geometry)
  MutationDebtXhci :
    XhciController failedPhase device generation (Just geometry) ->
    XhciDebtReceipt device generation failedPhase ->
    XhciController XhciMutationDebt device generation (Just geometry)
  QuarantinedXhci :
    XhciController XhciMutationDebt device generation (Just geometry) ->
    XhciQuarantineReceipt device generation ->
    XhciController XhciQuarantined device generation (Just geometry)

public export
claimXhci :
  MatchCertificate device ->
  LiveXhciAuthorization device generation ->
  XhciController XhciClaimed device generation Nothing
claimXhci = ClaimedXhci

public export
observeProvisionalCapability :
  XhciController XhciClaimed device generation Nothing ->
  ProvisionalCapabilityReceipt device generation geometry ->
  XhciController
    XhciCapabilityProvisional device generation (Just geometry)
observeProvisionalCapability = ProvisionalXhci

public export
resolveXhciFirmware :
  XhciController
    XhciCapabilityProvisional device generation (Just geometry) ->
  FirmwareResolutionReceipt device generation ->
  XhciController XhciFirmwareResolved device generation (Just geometry)
resolveXhciFirmware = FirmwareResolvedXhci

public export
recordXhciHalted :
  XhciController XhciFirmwareResolved device generation (Just geometry) ->
  HaltReceipt device generation ->
  XhciController XhciHalted device generation (Just geometry)
recordXhciHalted = HaltedXhci

public export
measureXhciAperture :
  XhciController XhciHalted device generation (Just geometry) ->
  MeasuredApertureReceipt device generation geometry ->
  XhciController XhciApertureMeasured device generation (Just geometry)
measureXhciAperture = ApertureMeasuredXhci

public export
recordXhciResetReady :
  XhciController XhciApertureMeasured device generation (Just geometry) ->
  ResetReadyReceipt device generation ->
  XhciController XhciResetReady device generation (Just geometry)
recordXhciResetReady = ResetReadyXhci

public export
recordXhciProtocolMap :
  XhciController XhciResetReady device generation (Just geometry) ->
  ProtocolMapReceipt device generation ->
  XhciController XhciProtocolMapped device generation (Just geometry)
recordXhciProtocolMap = ProtocolMappedXhci

public export
recordXhciRingsReady :
  XhciController XhciProtocolMapped device generation (Just geometry) ->
  RingReadyReceipt device generation ->
  XhciController XhciRingsReady device generation (Just geometry)
recordXhciRingsReady = RingsReadyXhci

public export
deferXhciOperational :
  XhciController XhciRingsReady device generation (Just geometry) ->
  DeferredOperationalReceipt device generation ->
  XhciController XhciOperationalDeferred device generation (Just geometry)
deferXhciOperational = OperationalDeferredXhci

public export
recordXhciMutationDebt :
  XhciController failedPhase device generation (Just geometry) ->
  XhciDebtReceipt device generation failedPhase ->
  XhciController XhciMutationDebt device generation (Just geometry)
recordXhciMutationDebt = MutationDebtXhci

public export
quarantineXhciDebt :
  XhciController XhciMutationDebt device generation (Just geometry) ->
  XhciQuarantineReceipt device generation ->
  XhciController XhciQuarantined device generation (Just geometry)
quarantineXhciDebt = QuarantinedXhci

public export
data XhciRetryPermission : XhciPhase -> Type where
  RetryFreshClaim : XhciRetryPermission XhciClaimed

public export
data XhciReleasePermission : XhciPhase -> Type where
  ReleaseUnmutatedClaim : XhciReleasePermission XhciClaimed
  ReleaseReadOnlyObservation :
    XhciReleasePermission XhciCapabilityProvisional

public export
xhciDebtCannotRetry : XhciRetryPermission XhciMutationDebt -> Void
xhciDebtCannotRetry permission impossible

public export
xhciQuarantineCannotRetry : XhciRetryPermission XhciQuarantined -> Void
xhciQuarantineCannotRetry permission impossible

public export
xhciDebtCannotRelease : XhciReleasePermission XhciMutationDebt -> Void
xhciDebtCannotRelease permission impossible

public export
xhciQuarantineCannotRelease :
  XhciReleasePermission XhciQuarantined -> Void
xhciQuarantineCannotRelease permission impossible

-- VT-d firmware tables often describe a remapping unit shared by several PCI
-- requesters.  Sharing a unit is not permission to publish several contexts:
-- the admission decision below constructs one context target, indexed by the
-- selected requester.  This mirrors the Rust backend's empty-table,
-- single-context construction policy.

public export
data DmarScopePolicy
  = FirmwareSingle
  | IsolatedIncludeAll
  | IsolatedSharedUnit

public export
data DmarScopeFault
  = NonZeroPciSegment
  | UnresolvedRequesterScope
  | FirmwareRouteMismatch
  | IncludeAllHasExplicitEndpoints
  | MissingExplicitEndpoint
  | RequesterOutsideExplicitScope

public export
record RawDmarScope where
  constructor MkRawDmarScope
  segmentZero : Bool
  includeAll : Bool
  unresolvedRequesterScopes : Bool
  explicitEndpoints : List Nat
  routedRequester : Nat

-- A context table may contain an entry only for its type-indexed requester.
-- There is deliberately no constructor that names a different requester.
public export
data PublishedContext : Nat -> Nat -> Type where
  PublishSelectedRequester : PublishedContext requester requester

public export
record ScopedDmaCertificate (requester : Nat) where
  constructor MkScopedDmaCertificate
  policy : DmarScopePolicy
  routedRequester : Nat
  requesterAgreement : requester = routedRequester
  tablesEmptyBeforePublication : IsTrue True
  publishedContext : PublishedContext requester requester

public export
data ScopedDmaDecision : Nat -> Type where
  ScopeRejected : DmarScopeFault -> ScopedDmaDecision requester
  ScopeAccepted : ScopedDmaCertificate requester -> ScopedDmaDecision requester

containsRequester : Nat -> List Nat -> Bool
containsRequester requester [] = False
containsRequester requester (candidate :: rest) =
  case decEq requester candidate of
    Yes Refl => True
    No different => containsRequester requester rest

admitScopedDma :
  (requester : Nat) ->
  (policy : DmarScopePolicy) ->
  ScopedDmaDecision requester
admitScopedDma requester policy =
  ScopeAccepted
    (MkScopedDmaCertificate
      policy
      requester
      Refl
      Proven
      PublishSelectedRequester)

verifyExplicitScope :
  (requester : Nat) ->
  List Nat ->
  ScopedDmaDecision requester
verifyExplicitScope requester [] = ScopeRejected MissingExplicitEndpoint
verifyExplicitScope requester (endpoint :: []) =
  case decEq requester endpoint of
    Yes Refl => admitScopedDma requester FirmwareSingle
    No different => ScopeRejected RequesterOutsideExplicitScope
verifyExplicitScope requester endpoints =
  case containsRequester requester endpoints of
    True => admitScopedDma requester IsolatedSharedUnit
    False => ScopeRejected RequesterOutsideExplicitScope

public export
verifyDmarScope :
  (requester : Nat) ->
  RawDmarScope ->
  ScopedDmaDecision requester
verifyDmarScope requester
  (MkRawDmarScope False includeAll unresolved endpoints routed) =
    ScopeRejected NonZeroPciSegment
verifyDmarScope requester
  (MkRawDmarScope True includeAll True endpoints routed) =
    ScopeRejected UnresolvedRequesterScope
verifyDmarScope requester
  (MkRawDmarScope True True False [] routed) =
    case decEq requester routed of
      Yes Refl => admitScopedDma requester IsolatedIncludeAll
      No different => ScopeRejected FirmwareRouteMismatch
verifyDmarScope requester
  (MkRawDmarScope True True False endpoints routed) =
    ScopeRejected IncludeAllHasExplicitEndpoints
verifyDmarScope requester
  (MkRawDmarScope True False False endpoints routed) =
    case decEq requester routed of
      Yes Refl => verifyExplicitScope requester endpoints
      No different => ScopeRejected FirmwareRouteMismatch

public export
publishedContextCannotNameAnotherRequester :
  PublishedContext requester requester ->
  (other : Nat) ->
  Not (requester = other) ->
  Not (PublishedContext requester other)
publishedContextCannotNameAnotherRequester PublishSelectedRequester other different
  PublishSelectedRequester = different Refl

public export
sharedUnitExample : ScopedDmaDecision 17
sharedUnitExample =
  verifyDmarScope 17
    (MkRawDmarScope True False False [4, 17, 99] 17)
