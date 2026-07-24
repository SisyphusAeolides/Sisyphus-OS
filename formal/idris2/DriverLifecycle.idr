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
