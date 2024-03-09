#!/bin/bash

delay=$1
qsize=$2
loss=$3
cca=$4
trace=$5
port=$6
log_path=$7
result_path=$8
MAX_CONCURR_REQ=$9
MAX_REQS=${10}
WORKLOAD_FILE=${11}


./target/release/tquic_server --log-level Error -c cert.crt -k cert.key -l 0.0.0.0:$port -C $cca > /dev/null &
server_pid=$!
# shellcheck disable=SC2086
mm-delay $delay mm-loss downlink $loss mm-link $trace $trace  --uplink-queue=droptail --downlink-queue=droptail --uplink-queue-args="packets=200" --downlink-queue-args="packets=$qsize"  -- ./client.sh $port $MAX_REQS $MAX_CONCURR_REQ $WORKLOAD_FILE $result_path
sudo kill $server_pid
echo "$trace" >> "$log_path"
grep 'mean:' "$result_path" | tail -n 1 >> "$log_path"