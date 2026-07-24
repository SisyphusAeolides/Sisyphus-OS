module PackageTransaction

%default total

public export
record Package where
  constructor MkPackage
  packageName : String
  versionIndex : Nat

public export
data Mutation
  = Install Package
  | Remove String Nat
  | Upgrade String Nat Nat

public export
data TransactionFault
  = ContradictoryIntent
  | AlreadyInstalled
  | NotInstalled
  | VersionPreconditionFailed

mutationName : Mutation -> String
mutationName (Install package) = packageName package
mutationName (Remove name expected) = name
mutationName (Upgrade name expected replacement) = name

touches : String -> Mutation -> Bool
touches name mutation = name == mutationName mutation

hasContradiction : List Mutation -> Bool
hasContradiction [] = False
hasContradiction (mutation :: rest) =
  any (touches (mutationName mutation)) rest || hasContradiction rest

findPackage : String -> List Package -> Maybe Package
findPackage name [] = Nothing
findPackage name (package :: rest) =
  if packageName package == name
    then Just package
    else findPackage name rest

removeExact : String -> Nat -> List Package -> Either TransactionFault (List Package)
removeExact name expected [] = Left NotInstalled
removeExact name expected (package :: rest) =
  if packageName package == name
    then if versionIndex package == expected
      then Right rest
      else Left VersionPreconditionFailed
    else case removeExact name expected rest of
      Left fault => Left fault
      Right retained => Right (package :: retained)

upgradeExact :
  String -> Nat -> Nat -> List Package ->
  Either TransactionFault (List Package)
upgradeExact name expected replacement [] = Left NotInstalled
upgradeExact name expected replacement (package :: rest) =
  if packageName package == name
    then if versionIndex package == expected
      then Right (MkPackage name replacement :: rest)
      else Left VersionPreconditionFailed
    else case upgradeExact name expected replacement rest of
      Left fault => Left fault
      Right upgraded => Right (package :: upgraded)

applyMutation : List Package -> Mutation -> Either TransactionFault (List Package)
applyMutation packages (Install package) =
  case findPackage (packageName package) packages of
    Just installed => Left AlreadyInstalled
    Nothing => Right (packages ++ [package])
applyMutation packages (Remove name expected) =
  removeExact name expected packages
applyMutation packages (Upgrade name expected replacement) =
  upgradeExact name expected replacement packages

applyAll : List Package -> List Mutation -> Either TransactionFault (List Package)
applyAll packages [] = Right packages
applyAll packages (mutation :: rest) =
  case applyMutation packages mutation of
    Left fault => Left fault
    Right next => applyAll next rest

public export
record Ledger (generation : Nat) where
  constructor MkLedger
  installed : List Package

public export
data TransactionAuthority : Nat -> Type where
  IssuedAuthority : TransactionAuthority generation

public export
record StagedTransaction (generation : Nat) where
  constructor MkStagedTransaction
  before : Ledger generation
  after : List Package
  intentions : List Mutation

public export
data StagingResult : Nat -> Type where
  StagingRejected : TransactionFault -> Ledger generation -> StagingResult generation
  StagingReady : StagedTransaction generation -> StagingResult generation

public export
stage : Ledger generation -> List Mutation -> StagingResult generation
stage ledger intentions =
  if hasContradiction intentions
    then StagingRejected ContradictoryIntent ledger
    else case applyAll (installed ledger) intentions of
      Left fault => StagingRejected fault ledger
      Right next => StagingReady (MkStagedTransaction ledger next intentions)

public export
record CommitResult (generation : Nat) where
  constructor MkCommitResult
  committedLedger : Ledger (S generation)
  nextAuthority : TransactionAuthority (S generation)

public export
commit :
  TransactionAuthority generation ->
  StagedTransaction generation ->
  CommitResult generation
commit IssuedAuthority transaction =
  MkCommitResult (MkLedger (after transaction)) IssuedAuthority

public export
generationAdvances : CommitResult generation -> Ledger (S generation)
generationAdvances = committedLedger

public export
sampleStage : StagingResult 0
sampleStage =
  stage
    (MkLedger [])
    [ Install (MkPackage "boulder" 1)
    , Install (MkPackage "crest" 3)
    ]

public export
sampleCommit : Maybe (CommitResult 0)
sampleCommit =
  case sampleStage of
    StagingRejected fault original => Nothing
    StagingReady transaction => Just (commit IssuedAuthority transaction)
