#!/bin/bash
port=$1
max_requests_per_thread=$2
max_concurrent_requests=$3
workload=$4
result_path=$5

# shellcheck disable=SC2086
./target/release/tquic_client --log-level Info --connect-to $MAHIMAHI_BASE:$port https://example.org --max-requests-per-thread $max_requests_per_thread --max-requests-per-conn $max_requests_per_thread  --max-concurrent-requests $max_concurrent_requests --workload-trace $workload -C BBR  >> $result_path 2>&1