------------------------------ MODULE CellCoordination ------------------------------
EXTENDS Naturals, FiniteSets, TLC

CONSTANTS Nodes, Commands, FaultMode
NoOwner == "none"

VARIABLES owner, epoch, state, accepted, acknowledged, retained, published,
    admitted, quiescing, recovery_required, released
vars == <<owner, epoch, state, accepted, acknowledged, retained, published,
    admitted, quiescing, recovery_required, released>>

Serving == "serving"
Fenced == "fenced"
Idle == "idle"

Init ==
    /\ owner = NoOwner
    /\ epoch = 0
    /\ state = [n \in Nodes |-> Idle]
    /\ accepted = [c \in Commands |-> FALSE]
    /\ acknowledged = [c \in Commands |-> FALSE]
    /\ retained = FALSE
    /\ published = 0
    /\ admitted = [c \in Commands |-> FALSE]
    /\ quiescing = FALSE
    /\ recovery_required = FALSE
    /\ released = FALSE

Admit(n, c) ==
    /\ owner = n
    /\ state[n] = Serving
    /\ ~quiescing
    /\ ~accepted[c]
    /\ accepted' = [accepted EXCEPT ![c] = TRUE]
    /\ admitted' = [admitted EXCEPT ![c] = TRUE]
    /\ UNCHANGED <<owner, epoch, state, acknowledged, retained, published,
        quiescing, recovery_required, released>>

Prepare(n) ==
    /\ owner = n
    /\ state[n] = Serving
    /\ retained = FALSE
    /\ published < 2
    /\ \E c \in Commands: accepted[c] /\ ~acknowledged[c]
    /\ retained' = TRUE
    /\ UNCHANGED <<owner, epoch, state, accepted, acknowledged, published,
        admitted, quiescing, recovery_required, released>>

Publish(n) ==
    /\ owner = n
    /\ state[n] = Serving
    /\ retained = TRUE
    /\ published < 2
    /\ retained' = FALSE
    /\ published' = published + 1
    /\ recovery_required' = FALSE
    /\ UNCHANGED <<owner, epoch, state, accepted, acknowledged,
        admitted, quiescing, released>>

Ack(c) ==
    /\ accepted[c]
    /\ published > 0
    /\ ~acknowledged[c]
    /\ acknowledged' = [acknowledged EXCEPT ![c] = TRUE]
    /\ UNCHANGED <<owner, epoch, state, accepted, retained, published,
        admitted, quiescing, recovery_required, released>>

Renew(n) ==
    /\ owner = n
    /\ state[n] = Serving
    /\ UNCHANGED vars

Fence(n) ==
    /\ owner = n
    /\ state' = [state EXCEPT ![n] = Fenced]
    /\ quiescing' = TRUE
    /\ UNCHANGED <<owner, epoch, accepted, acknowledged, retained, published,
        admitted, recovery_required, released>>

Drain(n) ==
    /\ owner = n
    /\ state[n] = Serving
    /\ ~quiescing
    /\ quiescing' = TRUE
    /\ UNCHANGED <<owner, epoch, state, accepted, acknowledged, retained,
        published, admitted, recovery_required, released>>

Crash(n) ==
    /\ owner = n
    /\ state' = [state EXCEPT ![n] = Fenced]
    /\ owner' = NoOwner
    /\ recovery_required' = (recovery_required \/ retained)
    /\ quiescing' = TRUE
    /\ UNCHANGED <<epoch, accepted, acknowledged, retained, published,
        admitted, released>>

Takeover(n) ==
    /\ owner = NoOwner \/ \E m \in Nodes: m = owner /\ state[m] = Fenced
    /\ n \in Nodes
    /\ epoch < 2
    /\ owner' = n
    /\ epoch' = epoch + 1
    /\ state' = [m \in Nodes |-> IF m = n THEN Serving ELSE Idle]
    /\ quiescing' = FALSE
    /\ released' = FALSE
    /\ UNCHANGED <<accepted, acknowledged, retained, published,
        admitted, recovery_required>>

Release(n) ==
    /\ owner = n
    /\ state[n] = Serving
    /\ ~retained
    /\ ~recovery_required
    /\ \A c \in Commands: ~accepted[c] \/ acknowledged[c]
    /\ owner' = NoOwner
    /\ state' = [state EXCEPT ![n] = Idle]
    /\ released' = TRUE
    /\ UNCHANGED <<epoch, accepted, acknowledged, retained, published,
        admitted, quiescing, recovery_required>>

EarlyAck(c) ==
    /\ FaultMode = "early_ack"
    /\ accepted[c]
    /\ ~acknowledged[c]
    /\ acknowledged' = [acknowledged EXCEPT ![c] = TRUE]
    /\ UNCHANGED <<owner, epoch, state, accepted, retained, published,
        admitted, quiescing, recovery_required, released>>

DualOwner(n) ==
    /\ FaultMode = "dual_owner"
    /\ owner # NoOwner
    /\ n \in Nodes
    /\ n # owner
    /\ owner' = n
    /\ state' = [state EXCEPT ![n] = Serving]
    /\ UNCHANGED <<epoch, accepted, acknowledged, retained, published,
        admitted, quiescing, recovery_required, released>>

AdoptDifferentWinner(n) ==
    /\ FaultMode = "different_winner"
    /\ owner # NoOwner
    /\ n \in Nodes
    /\ n # owner
    /\ owner' = n
    /\ UNCHANGED <<epoch, state, accepted, acknowledged, retained, published,
        admitted, quiescing, recovery_required, released>>

EarlyRelease(n) ==
    /\ FaultMode = "early_release"
    /\ owner = n
    /\ retained
    /\ owner' = NoOwner
    /\ state' = [state EXCEPT ![n] = Idle]
    /\ UNCHANGED <<epoch, accepted, acknowledged, retained, published,
        admitted, quiescing, recovery_required, released>>

NormalNext ==
    \/ \E n \in Nodes, c \in Commands: Admit(n, c)
    \/ \E n \in Nodes: Prepare(n)
    \/ \E n \in Nodes: Publish(n)
    \/ \E c \in Commands: Ack(c)
    \/ \E n \in Nodes: Renew(n)
    \/ \E n \in Nodes: Drain(n)
    \/ \E n \in Nodes: Fence(n)
    \/ \E n \in Nodes: Crash(n)
    \/ \E n \in Nodes: Takeover(n)
    \/ \E n \in Nodes: Release(n)

FaultNext ==
    \/ \E c \in Commands: EarlyAck(c)
    \/ \E n \in Nodes: DualOwner(n)
    \/ \E n \in Nodes: AdoptDifferentWinner(n)
    \/ \E n \in Nodes: EarlyRelease(n)

Next == NormalNext \/ FaultNext

NoDualServing == Cardinality({n \in Nodes: state[n] = Serving}) <= 1
AckHasDurability == \A c \in Commands: acknowledged[c] => published > 0
RetainedHasOwner == retained => owner # NoOwner \/ recovery_required
OwnerIsLive == owner = NoOwner \/ \E n \in Nodes: n = owner /\ state[n] # Idle
EpochIsNatural == epoch \in Nat
EpochMonotonic == [] [epoch' >= epoch]_vars
PublishedIsNatural == published \in Nat
PublishedMonotonic == [] [published' >= published]_vars
NoAdmissionAfterFence == [] [(
    \A n \in Nodes: state[n] = Fenced => accepted' = accepted
)]_vars
ReleaseAfterDurability == [] (released =>
    /\ ~retained
    /\ ~recovery_required
    /\ \A c \in Commands: ~accepted[c] \/ acknowledged[c])
RetainedRecoveryObligation == [] (recovery_required =>
    retained \/ published > 0)
DrainOrRecoveryReady == [] (released => owner = NoOwner)

(* Under the normal (non-fault) bounded provider contract, every admitted
   publication/acknowledgement step and the terminal release step eventually
   get a scheduling opportunity.  This is deliberately a bounded liveness
   claim: it does not model provider timing or unbounded membership. *)
FairProgress ==
    /\ WF_vars(\E n \in Nodes: Prepare(n))
    /\ WF_vars(\E n \in Nodes: Publish(n))
    /\ WF_vars(\E c \in Commands: Ack(c))
    /\ WF_vars(\E n \in Nodes: Takeover(n))
    /\ WF_vars(\E n \in Nodes: Release(n))

DrainCompletes ==
    []((quiescing /\ owner # NoOwner) ~> (owner = NoOwner \/ released))

Spec == Init /\ [][Next]_vars

(* Liveness is checked under an explicit stable-provider profile. Fence and
   Crash remain in the safety model, but a liveness claim cannot promise
   completion while an unbounded sequence of new owner failures is allowed. *)
StableNext ==
    \/ \E n \in Nodes, c \in Commands: Admit(n, c)
    \/ \E n \in Nodes: Prepare(n)
    \/ \E n \in Nodes: Publish(n)
    \/ \E c \in Commands: Ack(c)
    \/ \E n \in Nodes: Renew(n)
    \/ \E n \in Nodes: Drain(n)
    \/ \E n \in Nodes: Takeover(n)
    \/ \E n \in Nodes: Release(n)

LivenessSpec == Init /\ [][StableNext]_vars /\ FairProgress

THEOREM Spec => []EpochIsNatural
========================================================================================
