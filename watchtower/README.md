The `agave-watchtower` program is used to monitor the health of a cluster. It
periodically polls the cluster over an RPC API to confirm that the transaction
count is advancing, new blockhashes are available, and no validators are
delinquent. Results are reported as InfluxDB metrics, with an optional push
notification on sanity failure.

If you only care about the health of several specific validators, the
`--validator-identity` command-line argument can be used to restrict failure
notifications to issues only affecting that set of validators.

When the Validator Admission Ticket feature is active, watchtower also alerts
when a monitored validator's vote account balance is below the bank-side VAT
balance threshold computed from RPC-visible state. This check only verifies the
vote account balance over public RPC. It does not verify full VAT eligibility,
including BLS pubkey presence, stake, top validator set inclusion, or tie cutoff
behavior.

User can provide either 1 or 3 RPC URLs for the cluster via the `--url` or `--urls`
command-line arguments respectively. 2 URLs are not accepted because it's not enough
to have redundnacy, and more than 3 URLs are not accepted because there's little
benefit from having more than 3. If 3 URLs are provided, at least 2 of them have to
confirm health of a cluster.

### Metrics
#### `watchtower-sanity`
On every iteration this data point will be emitted indicating the overall result
using a boolean `ok` field.

#### `watchtower-sanity-failure`
On failure this data point contains details about the specific test that failed via
the following fields:
* `test`: name of the sanity test that failed
* `err`: exact sanity failure message
