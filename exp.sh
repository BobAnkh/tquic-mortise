#!/bin/bash

traces=("trace-2925703-home1"
    "trace-3458374-timessquare" "trace-3219061-home" "trace-3457194-timessquare" "trace-3201711-timessquare" "trace-3202253-timessquare" "trace-3205967-timessquare" "trace-2767958-taxi1" "trace-3205967-timessquare"
    "trace-3109898-bus")

# traces=("${traces[0]}")
traces=("trace-3458374-timessquare" )
# traces=("lte-trace1" "lte-trace2")
# ccas=("copa" "mvfst")
# ccas=("mortise" "mvfst" "bbr" "cubic" "bbr3")
ccas=("mortise")
# ccas=("$1")
# losses=(0.0 0.002 0.01)
losses=(0.0)
# losses=(0.001 0.005)
delays=(20 60 150)
# delays=(20)
# 对应的就是分子和分母
# buffers=(0.5 1 1.5)
qsize_coeff1=(1 2 6)
qsize_coeff2=(2 2 2)
# qsize_coeff1=(2)
# qsize_coeff2=(2)
iteration=2
# mortise可以根据当前并发流数是否用满，判断是否需要尽快迸发
MAX_CONCURR_REQ=20
MAX_REQS=500
WORKLOAD_FILE="./workload/workload-0.5s"
MAX_CONCURR_TASKS=4
LOG_ROOT_DIR="./test_cmp_0.5s"
TRACE_DIR="./traces/realworld/cellular"
# log_dir="logs"

cur_running_tasks=0
task_pids=()
sudo killall tquic_server tquic_client
sudo killall -9 mm-delay
sudo killall -9 mm-loss
sleep 5
for loss in "${losses[@]}"; do
    port=40002
    log_dir="$LOG_ROOT_DIR/max-con-$MAX_CONCURR_REQ/$loss"
    mkdir -p "$log_dir" 
    for trace in "${traces[@]}"; do
    for delay in "${delays[@]}"; do
        for ((idx=0; idx < "${#qsize_coeff1[@]}"; idx++)); do
            # 按平均吞吐为6Mbps计算   
            fenzi="${qsize_coeff1[$idx]}"
            fenmu="${qsize_coeff2[$idx]}"
            qsize=$((2 * delay * 4 * fenzi / 12 / fenmu))
            # qsize=40
            echo "queue: $qsize"
            for cca in "${ccas[@]}"; do
            # shellcheck disable=SC2086
                log_path="$log_dir/$delay-$idx-$cca-v2"
                result_path="$log_dir"/"$cca-v2"
                    trace_path="$TRACE_DIR/$trace"
                    for ((iter = 0; iter < iteration; iter++)); do
                        port=$((port + 1))
                        # shellcheck disable=SC2086
                        # shellcheck disable=SC2094
                        if [[ "$cur_running_tasks" -ge "$MAX_CONCURR_TASKS" ]]; then
                            ./run_single_exp.sh $delay $qsize $loss $cca $trace_path $port $log_path $result_path $MAX_CONCURR_REQ $MAX_REQS $WORKLOAD_FILE
                            for pid in "${task_pids[@]}"; do
                                wait "$pid"
                            done
                            # exit 1
                            echo "[Multitask] $MAX_CONCURR_TASKS Tasks done"
                            cur_running_tasks=0
                            task_pids=()
                        else
                            ./run_single_exp.sh $delay $qsize $loss $cca $trace_path $port $log_path $result_path $MAX_CONCURR_REQ $MAX_REQS $WORKLOAD_FILE &
                            task_pids+=($!)
                            cur_running_tasks=$((cur_running_tasks + 1))
                            echo "[Multitask] $cur_running_tasks Tasks running..."
                            sleep 1
                        fi
                    done
                done
            done
        done
    done
done
