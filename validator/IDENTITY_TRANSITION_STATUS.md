# Identity transition observations

`identityTransitionStatus` is a parameterless JSON-RPC method exposed by minimal
HTTP RPC and local admin IPC. Both return the same validator-owned observation.
It changes neither `setIdentity` nor `setIdentityFromBytes` return values or
consensus behavior.

```text
Admin request                   Voting context               Observation
setIdentity(B)                  still uses A                 transitioning A -> B
publish B                       still uses A                 transitioning A -> B
return success                  load B's consensus state     transitioning A -> B
                                adopt B                     complete A -> B
                                                            old submission slot frozen
```

The identity command may return before the voting context adopts the requested
identity. Conversely, adoption may occur before the command returns. Completion
is published only after both succeed.

## Response

```json
{
  "version": 1,
  "processInstanceId": "validator-instance-identifier",
  "sequence": 42,
  "state": "complete",
  "consensus": "tower",
  "currentIdentity": "B",
  "fromIdentity": "A",
  "toIdentity": "B",
  "voteAccount": "vote-account-pubkey",
  "fromIdentityLastSubmittedVoteSlot": 123,
  "towerRootSlot": 90,
  "error": null
}
```

Real identities and vote accounts are base58 public keys. `currentIdentity` is
the currently published cluster identity, which may change before completion.
Sequences are scoped to `processInstanceId`, not a machine or ledger directory.

States are `idle`, `transitioning`, `complete`, and `failed`. Consensus is
`unknown`, `tower`, or `alpenglow`. `failed` can mean a command/adoption error OR
that an authoritative observation is unavailable; it never rejects a command.

`fromIdentityLastSubmittedVoteSlot` is the highest vote slot successfully
accepted by the outbound voting channel under the old identity's current voting
context. Refresh submissions count. Generated votes, locally ingested votes,
restored history, full-channel drops, and failed channel sends do not count.
The field is nullable: `null` means no submission was observed in that context;
slot zero is a distinct valid observation. No history is reconstructed after
restart. This record is not a complete persisted vote history.

```text
Voting A -> non-voting B: complete, old submission slot S (if observed)
Non-voting B -> voting A: complete, old submission slot null (if none observed)
                         A's later votes do not alter that completed record
```

Completion establishes successful identity adoption and cessation of new vote
creation under the old context. It does NOT establish transmission, landing,
finalization, queue drainage, final tower persistence, or destination readiness.
Consumers needing finality must query suitable consensus-specific finalized
state; `getSlot` alone does not prove a vote landed. Consumers transferring
consensus state must independently ensure they copy the final persisted state.

Tower observations acknowledge after new-tower loading succeeds. Alpenglow
observations acknowledge after identity handling and restored-history
initialization succeed. `towerRootSlot` is absent for Alpenglow and may be absent
during startup. Alpenglow submission slots are not Tower vote-account evidence.

## Conservative cases

Same-identity commands keep their original behavior, but report `failed` because
they have no identity-adoption barrier. Missing post-initialization admin metadata
returns the existing startup retry error; HTTP can return `idle` before a request.

Overlapping/unacknowledged changes can coalesce in existing voting loops. They
continue executing, but disable authoritative completion until validator restart.
This avoids matching an old acknowledgement to a repeated destination identity.

Observation is unavailable during consensus migration, including the initial
Alpenglow epoch before the full Alpenglow epoch. That interval includes vote
producers whose old-context submissions cannot be represented authoritatively
by one Tower or Votor watermark. Commands still run as before.

## Cost

Ordinary submissions update only a local optional slot. No observation lock,
allocation, thread, file write, or extra message is added per vote. Locks and
response formatting are confined to identity transitions and queries. Refresh
batches additionally inspect vote slots to find their maximum.

Use the `identity_transition_bench` example for queue/submission, transition, and
snapshot microbenchmarks. These do not replace validator replay/voting profiles.
