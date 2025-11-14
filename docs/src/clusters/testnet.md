# Test Network Management
The `./net/` directory in the monorepo contains scripts useful for creation and manipulation of a test network.
The test network allows you to run a fully isolated set of validators and clients on a configurable hardware setup.
It's intended to be both dev and CD friendly.


### Cloud account prerequisites

The test networks to be created can run in GCP, AWS or colo. Whichever cloud provider you choose, you will need the credentials set up on your machine.

#### GCP
You will need a working `gcloud` command from google SDK,
if you do not have it follow the guide in [https://cloud.google.com/sdk?hl=en](https://cloud.google.com/sdk?hl=en)

Before running any scripts, authenticate with
```bash
$ gcloud auth login
```
If you are running the scripts on a headless machine, you can use curl to issue requests to confirm your auth.

If you are doing it the first time, you might need to set up project
```bash
gcloud config set project principal-lane-200702
```

#### AWS
Obtain your credentials from the AWS IAM Console and configure the AWS CLI with
```bash
$ aws configure
```
More information on AWS CLI configuration can be found [here](https://docs.aws.amazon.com/cli/latest/userguide/cli-chap-getting-started.html#cli-quick-configuration)

## Metrics configuration (Optional)
Metrics collection relies on 2 environment variables that are patched to the remote nodes by net.sh:
 * `RUST_LOG` to enable metrics reporting in principle
 * `SOLANA_METRICS_CONFIG` to tell agave where to log the metrics

### Preparation
> [!NOTE]
> Anza employees should follow the guide in notion to set up the influxDB account.

 * Ensure that `${host}` is the host name of the InfluxDB you can access, for example `https://internal-metrics.solana.com:8086`
 * Ensure that `${user}` is the name of an InfluxDB user account with enough
rights to create a new InfluxDB database, for example `solana`.

### To set up the metrics
You will normally only need to do this once. Once this is done, you will be able to save the metrics configuration and load it later from the environment.

* Go to ./net/ in agave repo
* Run `./init-metrics.sh -c testnet-dev-${user} ${user} `
  * Script will ask for a password, it is the same one you’ve created when making a user in the InfluxDB UI
  * Put the username you have used in preparation, not your login user name
  * If you need to set influxDb host, edit the script
* The script will configure the database (recreating one if necessary) and append a config line in the very end of `net/config/config` file like the following:
  * `export SOLANA_METRICS_CONFIG="host=${host},db=testnet-dev-${user},u=${user},p=some_secret"`
  * You can store that line somewhere and append it to the config file when you need to reuse the database.
  * You can also store it into your shell’s environment so you can run `./init-metrics.sh -e` to quickly load it
  * Alternatively, you'll need to run `./init-metrics.sh` with appropriate arguments every time you set up a new cluster
* Assuming no errors, your influxDB setup is now done.
* For simple cases, storing `SOLANA_METRICS_CONFIG` in your env is appropriate, but you may want to use different databases for different runs of net.sh
  * You can call ./init-metrics.sh before you call net.sh start, this will change the metrics config for a particular run.
  * You can manually write `SOLANA_METRICS_CONFIG` in the `./net/config/config` file
* By default, metrics are only logged by agave if `RUST_LOG` is set to `info` or higher. You can provide it as environment for `./net.sh start` command, or set this in your shell environment.
  ```bash
  RUST_LOG="solana_metrics=info"
  ```

### To validate that your database and metrics environment variables are set up 100% correctly

Note: this only works if you store `SOLANA_METRICS_CONFIG` in your shell environment

```bash
  cd ./scripts/
  source  ./configure-metrics.sh
    INFLUX_HOST=https://internal-metrics.solana.com:8086
    INFLUX_DATABASE=testnet-dev-solana
    INFLUX_USERNAME=solana
    INFLUX_PASSWORD=********
  ./metrics-write-datapoint.sh "testnet-deploy net-create-begin=1"

  ```
  * All commands should complete with no errors, this indicates your influxDB config is usable
  * Ensure that `RUST_LOG` is set to `info` or `debug`

## Quick Start

NOTE: This example uses GCE.  If you are using AWS EC2, replace `./gce.sh` with
`./ec2.sh` in the commands.

```bash
# In Agave repo
cd net/

# Create a GCE testnet with 4 additional validator nodes (beyond the bootstrap node) and 1 client (billing starts here)
./gce.sh create -n 4 -c 1

# Configure the metrics database and validate credentials using environment variable `SOLANA_METRICS_CONFIG` (skip this if you are not using metrics)
./init-metrics.sh -c testnet-dev-${USER} ${USER}

# Deploy the network from the local workspace and start processes on all nodes including bench-tps on the client node
RUST_LOG=info ./net.sh start

# Show a help to ssh into any testnet node to access logs/etc
./ssh.sh

# Stop running processes on all nodes
./net.sh stop

# Dispose of the network (billing stops here)
./gce.sh delete
```

## Full guide
* If you expect metrics to work, make sure you have configured them before proceeding
* Go to `./net/` directory in agave repo
* `./gce.sh` command controls creation and destruction of the nodes in the test net. It does not actually run any software.
  * `./gce.sh create \-n 4 \-c 2` creates cluster with 4 validators and 1 node for load generation, this is minimal viable setup for all solana features to work
    * If the creation succeeds, `net/config/config` will contain the config file of the testnet just created
    * If you do not have `SOLANA_METRICS_CONFIG` set in your shell env, `gce.sh` may complain about metrics not being configured, this is perfectly fine
  * `./gce.sh info`  lists active test cluster nodes, this allows you to get their IP addresses for SSH access and/or debugging
  * `./gce.sh delete`  destroys the nodes (save the electricity and $$$ - destroy your test nets the moment you no longer need them).
  * On GCE, if you do not delete nodes, they will self-destruct in 8 hours anyway, you can configure self-destruct timer by supplying `--self-destruct-hours=N` argument to `gce.sh`
  * On other cloud platforms the testnet will not self-destruct!
* To enable metrics in the testnet, at this point you need to either:
  * `./init-metrics.sh -c testnet-dev-${user} ${user}` to create a new metrics database from scratch
  * Manually set `SOLANA_METRICS_CONFIG` in `./net/config/config` (which is exactly what `init-metrics.sh` does for you)
  * `./init-metrics.sh -e` to load metrics config from `SOLANA_METRICS_CONFIG` into the testnet config file or
* `./net.sh` controls the payload on the testnet nodes, i.e. bootstrapping, the validators and bench-tps. In principle, you can run everything by hand, but `./net.sh` makes it easier.
  * `./net.sh start` to actually run the test network.
    * This will actually upload your current sources to the bootstrap host, build them there and upload the result to all the nodes
    * The script will take 5-10 of minutes to run, in the end it should print something like
     ```
     --- Deployment Successful
     Bootstrap validator deployment took 164 seconds
     ```
    * You can also make sure it logs successful test transfers:
    ```✅ 1 lamport(s) transferred: seq=0   time= 402ms signature=33uJtPJM6ekBGrWCgWHKw1TTQJVrLxYMe3sp2PUmSRVb21LyXn3nDbQmzsgQyihE7VP2zD2iR66Du8aDUnSSd6pb```
  * `./net.sh start  bench-tps=2="--tx_count 2500"` will start 2 clients with bench-tps workload sending 2500 transactions per batch.
    * --tx_count argument is passed to the bench-tps program, see its manual for more options
  * `./net.sh sanity`  to test the deployment, it is also run by start command
  * `./net.sh stop`  to stop the validators and client. This does not kill the machines, so you can study the logs etc.
  * `./net.sh start --nobuild` will skip the source compilation, you will generally want that if you are only changing configuration files rather than code, or just want to re-run the last test.
* To connect to the nodes:
  * `./gce.sh info ` to get the public IPs
  * `./ssh.sh <IP> ` to get a shell on the node
  * `sudo su` will give you root access on the nodes
  * Nodes run latest ubuntu LTS image
* You can also interact with the nodes using solana cli:
```bash
# source ip list  use as ${validatorIpList[4]}
source net/config/config

# airdrop
../target/release/solana -u http://${validatorIpList[1]}:8899 airdrop 1

# check feature
../target/release/solana -u http://${validatorIpList[1]}:8899 feature status

# activate a feature
../target/release/solana -u http://${validatorIpList[1]}:8899 feature activate <path to .json>

# check the stakes on current validators
../target/release/solana --url http://${validatorIpList[0]}:8899 validators
```

## Tips

### Automation
You will want to have a script like this pretty much immediately to avoid making mistakes in the init process:
```bash
# Create the testnet with reasonable node sizes for a small test
# This particular one will have 7 nodes: 1 bootstrap validator, 4 regular validators, and 2 clients
./gce.sh create -n4 -c2 --custom-machine-type "--machine-type n1-standard-16" --client-machine-type "--machine-type n1-standard-4"
# Patch metrics config from env into config file
./init-metrics.sh -e
# Enable metrics and start the network (this will also build software)
RUST_LOG=info ./net.sh start  -c bench-tps=2="--tx_count 25000"
```

### Inscrutable "nothing works everything times out state"
 Note that net.sh and `gce.sh info` commands do not actually check if all the nodes are still alive in gcloud,
 they just assume the config file information is correct. So if your nodes got killed/timed out they will lie to you. In such case, just use `gce.sh delete` to reset.

### Running the network over public IP addresses
By default private IP addresses are used with all instances in the same
availability zone to avoid GCE network egress charges. However to run the
network over public IP addresses:
```bash
$ ./gce.sh create -P ...
```
or
```bash
$ ./ec2.sh create -P ...
```

### Deploying a tarball-based network
To deploy the latest pre-built `edge` channel tarball (ie, latest from the `master`
branch), once the testnet has been created run:

```bash
$ ./net.sh start -t edge
```
