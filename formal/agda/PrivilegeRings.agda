{-# OPTIONS --safe --without-K #-}

module PrivilegeRings where

data Empty : Set where

Not : Set -> Set
Not proposition = proposition -> Empty

data Ring : Set where
  ring0 : Ring
  ring1 : Ring
  ring2 : Ring
  ring3 : Ring

-- Long mode's supervisor U/S bit cannot separate CPL1 from CPL2. The only
-- admitted domain constructors therefore bind every ring to a distinct page
-- table root; there is no constructor that places a non-kernel domain in the
-- kernel root or aliases the two supervisor compartments.
data AddressSpace : Set where
  kernelRoot : AddressSpace
  hardwareCellRoot : AddressSpace
  personalityRoot : AddressSpace
  userRoot : AddressSpace

data IsolatedDomain : Ring -> AddressSpace -> Set where
  kernelDomain : IsolatedDomain ring0 kernelRoot
  hardwareCellDomain : IsolatedDomain ring1 hardwareCellRoot
  personalityDomain : IsolatedDomain ring2 personalityRoot
  userDomain : IsolatedDomain ring3 userRoot

ring1CannotUseKernelRoot : Not (IsolatedDomain ring1 kernelRoot)
ring1CannotUseKernelRoot ()

ring2CannotUseKernelRoot : Not (IsolatedDomain ring2 kernelRoot)
ring2CannotUseKernelRoot ()

ring1CannotAliasRing2 : Not (IsolatedDomain ring1 personalityRoot)
ring1CannotAliasRing2 ()

ring2CannotAliasRing1 : Not (IsolatedDomain ring2 hardwareCellRoot)
ring2CannotAliasRing1 ()

-- The first index is the issuing ring and the second is the authority tier.
-- Constructors are the complete minting policy; Ring3 has no constructor for
-- any supervisor authority.
data Authority : Ring -> Ring -> Set where
  pid0Root : Authority ring0 ring0
  nativeDriverGrant : Authority ring0 ring1
  personalityGrant : Authority ring0 ring2
  userSelf : Authority ring3 ring3

ring3CannotMintRing0 : Not (Authority ring3 ring0)
ring3CannotMintRing0 ()

ring3CannotMintRing1 : Not (Authority ring3 ring1)
ring3CannotMintRing1 ()

ring3CannotMintRing2 : Not (Authority ring3 ring2)
ring3CannotMintRing2 ()

-- Every architecturally admitted cross-ring control transfer is explicit.
-- Cross-compartment Ring1/Ring2 traffic returns to Ring0 so that Ring0 can
-- change page tables before dispatching another compartment.
data AllowedGate : Ring -> Ring -> Set where
  ring3Syscall : AllowedGate ring3 ring0
  ring2KernelGate : AllowedGate ring2 ring0
  ring1KernelGate : AllowedGate ring1 ring0
  dispatchNativeDriver : AllowedGate ring0 ring1
  dispatchPersonality : AllowedGate ring0 ring2
  returnToUser : AllowedGate ring0 ring3

data ReturnMechanism : Set where
  iretq : ReturnMechanism
  sysretq : ReturnMechanism

-- IRETQ is the only admitted dispatch mechanism for CPL1/CPL2. SYSRETQ is
-- indexed exclusively by Ring3, matching the hardware instruction contract.
data DispatchMechanism : Ring -> ReturnMechanism -> Set where
  ring1Iretq : DispatchMechanism ring1 iretq
  ring2Iretq : DispatchMechanism ring2 iretq
  ring3Iretq : DispatchMechanism ring3 iretq
  ring3Sysretq : DispatchMechanism ring3 sysretq

ring1CannotSysret : Not (DispatchMechanism ring1 sysretq)
ring1CannotSysret ()

ring2CannotSysret : Not (DispatchMechanism ring2 sysretq)
ring2CannotSysret ()

-- Hardware authority exists only for a native Ring1 cell. Ring2 personality
-- translators and Ring3 processes must enter Ring0 brokers instead.
data HardwareGrant : Ring -> Set where
  portGrant : HardwareGrant ring1
  mmioGrant : HardwareGrant ring1
  iommuDmaGrant : HardwareGrant ring1

ring2CannotOwnHardware : Not (HardwareGrant ring2)
ring2CannotOwnHardware ()

ring3CannotOwnHardware : Not (HardwareGrant ring3)
ring3CannotOwnHardware ()

noRing3ToRing1Gate : Not (AllowedGate ring3 ring1)
noRing3ToRing1Gate ()

noRing3ToRing2Gate : Not (AllowedGate ring3 ring2)
noRing3ToRing2Gate ()

noRing2ToRing1Gate : Not (AllowedGate ring2 ring1)
noRing2ToRing1Gate ()

data ProcessIdentity : Set where
  pid0Process : ProcessIdentity
  nativeDriverProcess : ProcessIdentity
  personalityProcess : ProcessIdentity
  userProcess : ProcessIdentity

-- Termination is an indexed capability issued only for non-PID0 execution.
-- The absence of a PID0 constructor makes kernel-idle termination
-- unrepresentable while remaining inside Agda's strict safe fragment.
data Termination : ProcessIdentity -> Set where
  terminateNativeDriver : Termination nativeDriverProcess
  terminatePersonality : Termination personalityProcess
  terminateUser : Termination userProcess

pid0CannotTerminate : Not (Termination pid0Process)
pid0CannotTerminate ()

data PID0Step : Set where
  idleTick : PID0Step
  interruptWake : PID0Step
  scheduleUser : PID0Step

-- A user return belongs only to an ordinary Ring3 process. PID0 may dispatch
-- Ring3 through a gate, but can never itself become the returning process.
data UserReturn : ProcessIdentity -> Set where
  resumeUserProcess : UserReturn userProcess

pid0CannotReturnAsUser : Not (UserReturn pid0Process)
pid0CannotReturnAsUser ()
