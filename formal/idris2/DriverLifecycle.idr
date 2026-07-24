module DriverLifecycle

import Decidable.Equality

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
