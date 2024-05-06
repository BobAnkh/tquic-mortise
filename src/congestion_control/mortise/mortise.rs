// Copyright (c) 2023 The TQUIC Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Copa: Practical Delay-Based Congestion Control for the Internet.
//!
//! Copa is an end-to-end congestion control algorithm that detect the presence
//! of buffer-fillers by observing the delay evolution, and respond with
//! additive-increase/multiplicative decrease on specified parameters. Experimental
//! results show that Copa can achieve low queueing delay and excellent fairness with
//! other congestion control algorithms.
//!
//! See <https://web.mit.edu/copa/>.

use std::collections::VecDeque;
use std::io::Read;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::time::Duration;
use std::time::Instant;

use log::*;
use mortise_common::qoe::QuicAppInfo;
use mortise_common::report::ReportEntry;
use serde::de;
use serde::{Deserialize, Serialize};
use shared_memory::Shmem;
use shared_memory::ShmemConf;

use super::delivery_rate::DeliveryRateEstimator;
use super::minmax::MinMax;
use super::CongestionController;
use super::CongestionStats;
use crate::connection::rtt::RttEstimator;
use crate::connection::space::RateSamplePacketState;
use crate::connection::space::SentPacket;

/// Delta: determines how much to weigh delay compared to throughput.
const COPA_DELTA: f64 = 0.4;

/// Delta: determines how much to weigh delay compared to throughput during slow start
const SLOW_START_DELTA: f64 = 0.4;

/// Max count while cwnd grows with the same direction. Speed up if
/// the count exceeds threshold.
const SPEED_UP_THRESHOLD: u64 = 3;

/// Default standing rtt filter length.
const STANDING_RTT_FILTER_WINDOW: Duration = Duration::from_millis(100);

/// Default min rtt filter length.
const MIN_RTT_FILTER_WINDOW: Duration = Duration::from_secs(10);

/// Pacing gain to cope with ack compression.
const PACING_GAIN: u64 = 2;

/// Delay oscillation rate to check if queueing delay is nearly empty:
/// queueing_delay < 0.1 * (Rtt_max - Rtt_min)
/// Where Rtt_max and Rtt_min is the max and min RTT in the last 4 rounds.
const DELAY_OSCILLATION_THRESHOLD: f64 = 0.1;

/// Max loss rate in one round. If the loss rate exceeds the threshold, switch
/// the mode to competitive mode.
const LOSS_RATE_THRESHOLD: f64 = 0.1;

const USE_PREDICT: bool = true;

const USE_BOUNCE: bool = true;

const BOUNCE_INTERVALS: u64 = 8;

/// Copa configurable parameters.
#[derive(Debug)]
pub struct MortiseConfig {
    /// Minimal congestion window in bytes.
    min_cwnd: u64,

    /// Initial congestion window in bytes.
    initial_cwnd: u64,

    /// Initial Smoothed rtt.
    initial_rtt: Option<Duration>,

    /// Max datagram size in bytes.
    max_datagram_size: u64,

    /// Delta in slow start. Delta determines how much to weigh delay compared to
    /// throughput. A larger delta signifies that lower packet delays are preferable.
    slow_start_delta: f64,

    /// Delta in steady state.
    steady_delta: f64,

    /// Use rtt standing or latest rtt to calculate queueing delay.
    use_standing_rtt: bool,
}

impl MortiseConfig {
    pub fn new(
        min_cwnd: u64,
        initial_cwnd: u64,
        initial_rtt: Option<Duration>,
        max_datagram_size: u64,
    ) -> Self {
        Self {
            min_cwnd,
            initial_cwnd,
            initial_rtt,
            max_datagram_size,
            slow_start_delta: SLOW_START_DELTA,
            steady_delta: COPA_DELTA,
            use_standing_rtt: true,
        }
    }
}

impl Default for MortiseConfig {
    fn default() -> Self {
        Self {
            min_cwnd: 4 * crate::DEFAULT_SEND_UDP_PAYLOAD_SIZE as u64,
            initial_cwnd: 80 * crate::DEFAULT_SEND_UDP_PAYLOAD_SIZE as u64,
            initial_rtt: Some(crate::INITIAL_RTT),
            max_datagram_size: crate::DEFAULT_SEND_UDP_PAYLOAD_SIZE as u64,
            slow_start_delta: COPA_DELTA,
            steady_delta: COPA_DELTA,
            use_standing_rtt: true,
        }
    }
}

/// Copa congestion window growth direction.
#[derive(Eq, PartialEq, Debug)]
enum Direction {
    /// Cwnd increasing.
    Up,

    /// Cwnd decreasing.
    Down,
}

/// Copa competing mode with other flows.
#[derive(Eq, PartialEq, Debug)]
enum CompetingMode {
    /// Default mode, no competing flows.
    Default,

    /// Competitive mode, use adaptive delta.
    Competitive,
}

/// Velocity control states.
#[derive(Debug)]
struct Velocity {
    /// Cwnd growth direction.
    direction: Direction,

    /// Velocity coef.
    velocity: u64,

    /// Cwnd recorded at last time.
    last_cwnd: u64,

    /// Times while cwnd grows with the same direction. Speed up if cnt
    /// exceeds threshold.
    same_direction_cnt: u64,
}

impl Default for Velocity {
    fn default() -> Self {
        Self {
            direction: Direction::Up,
            velocity: 1,
            last_cwnd: 0,
            same_direction_cnt: 0,
        }
    }
}

/// Accumulate information from a single ACK/SACK.
#[derive(Debug)]
struct AckState {
    /// Ack time.
    now: Instant,

    /// Newly marked lost data size in bytes.
    newly_lost_bytes: u64,

    /// Newly acked data size in bytes.
    newly_acked_bytes: u64,

    /// Largest acked packet number.
    largest_acked_pkt_num: u64,

    /// Minimum rtt in this ACK packet.
    min_rtt: Duration,

    /// The last smoothed rtt in the current ACK packet.
    last_srtt: Duration,
}

impl Default for AckState {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            now,
            newly_lost_bytes: 0,
            newly_acked_bytes: 0,
            largest_acked_pkt_num: 0,
            min_rtt: Duration::ZERO,
            last_srtt: Duration::ZERO,
        }
    }
}

/// Round trip counter for tracking packet-timed round trips which starts
/// at the transmission of some segment, and then ends at the ack of that segment.
#[derive(Debug, Default)]
struct RoundTripCounter {
    /// Count of packet-timed round trips.
    pub round_count: u64,

    /// A boolean that BBR sets to true once per packet-
    /// timed round trip, on ACKs that advance BBR.round_count.
    pub is_round_start: bool,

    /// Packet number denoting the end of
    /// a packet-timed round trip.
    pub round_end_pkt_num: u64,

    /// Total acked bytes when a new round starts.
    pub last_total_acked_bytes: u64,

    /// Total lost bytes when a new round starts.
    pub last_total_lost_bytes: u64,

    /// Lost rate in this round, calculated as `lost / (lost + acked)`.
    pub loss_rate: f64,
}

/// Copa: Practical Delay-Based Congestion Control for the Internet.
///
/// See <https://web.mit.edu/copa/>.
pub struct Mortise {
    /// Config
    config: MortiseConfig,

    /// Statistics.
    stats: CongestionStats,

    /// The time origin point when Copa init, used for window filter updating.
    init_time: Instant,

    /// Competing mode.
    mode: CompetingMode,

    /// Is in slow start state.
    slow_start: bool,

    /// Congestion window/
    cwnd: u64,

    /// Velocity parameter, speeds-up convergence.
    velocity: Velocity,

    /// Weight factor for queueing delay. Use default value in default mode,
    /// and use an adaptive one in competitive mode.
    delta: f64,

    /// Minimum rtt in time window tau (srtt/2).
    standing_rtt_filter: MinMax,

    /// Minimum rtt in the last time period, e.g. 10s for default.
    min_rtt_filter: MinMax,

    /// Delivery rate estimator.
    delivery_rate_estimator: DeliveryRateEstimator,

    /// Ack state in the current round.
    ack_state: AckState,

    /// Whether cwnd should be increased.
    increase_cwnd: bool,

    /// Target pacing rate.
    target_rate: u64,

    /// The last sent packet number.
    last_sent_pkt_num: u64,

    /// Round trip counter.
    round: RoundTripCounter,

    /// To identify which connection is using this Copa.
    connection_trace_id: String,

    /// To identify the connection from user-space
    flow_id: u32,

    /// Connection to manager unix domain socket
    stream: UnixStream,

    /// Store user-defined shared memory handler
    shared_memory_handler: Option<Shmem>,

    /// Connection to python manager unix domain socket
    py_stream: Option<UnixStream>,

    /// Rate's Ewma, in MB/s
    rate_ewma: f64,

    /// Rate's Ewmv, in (MB/S)^2
    rate_ewmv: f64,

    /// last update ewma time
    last_update_ewma_time: Instant,

    intervals_cnt: u64,

    /// Unsent app data bytes
    unsent_bytes: u64,

    /// Rate residuals for ewma estimation
    rate_residuals: VecDeque<f64>,

    /// Most recent resp time
    recent_resp_time: Instant,

    /// Whether sending a large resp
    sending_large_resp: u8,

    /// current sending resp
    sending_resp: u64,

    /// total resps
    total_resp_cnts: u64,

    /// start sending time
    start_time: Instant,

    min_delta: f64,
}

impl std::fmt::Debug for Mortise {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("Copa")
            .field("config", &self.config)
            .field("stats", &self.stats)
            .field("init_time", &self.init_time)
            .field("mode", &self.mode)
            .field("slow_start", &self.slow_start)
            .field("cwnd", &self.cwnd)
            .field("velocity", &self.velocity)
            .field("delta", &self.delta)
            .field("standing_rtt_filter", &self.standing_rtt_filter)
            .field("min_rtt_filter", &self.min_rtt_filter)
            .field("ack_state", &self.ack_state)
            .field("increase_cwnd", &self.increase_cwnd)
            .field("target_rate", &self.target_rate)
            .field("last_sent_pkt_num", &self.last_sent_pkt_num)
            .field("round", &self.round)
            .field("connection_trace_id", &self.connection_trace_id)
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FlowOperation<'a> {
    Connect(&'a str),
    Disconnect(&'a str),
}

impl Drop for Mortise {
    fn drop(&mut self) {
        // Disconnect the flow with connection_trace_id through UDS. (Disconnect)
        let payload = FlowOperation::Disconnect(&self.connection_trace_id);
        let payload = serde_json::to_vec(&payload).unwrap();
        let len = (payload.len() as u32).to_be_bytes();
        self.stream.write_all(&len).unwrap();
        self.stream.write_all(&payload).unwrap();
    }
}

impl Mortise {
    pub fn new(config: MortiseConfig, connection_trace_id: String) -> Self {
        let slow_start_delta = config.slow_start_delta;
        let initial_cwnd = config.initial_cwnd;
        // Connect the flow with connection_trace_id through UDS. (Connect)
        let mut stream = UnixStream::connect("/tmp/mortise-quic.sock").unwrap();
        let payload = FlowOperation::Connect(&connection_trace_id);
        let payload = serde_json::to_vec(&payload).unwrap();
        let len = (payload.len() as u32).to_be_bytes();
        stream.write_all(&len).unwrap();
        stream.write_all(&payload).unwrap();
        let mut buf: [u8; 4] = [0; 4];
        // Read the length
        stream.read_exact(&mut buf).unwrap();
        // Read the exact flow_id
        stream.read_exact(&mut buf).unwrap();
        let flow_id = u32::from_be_bytes(buf);
        let mut py_stream = UnixStream::connect("/tmp/mortise-py.sock")
            .map_err(|e| log::error!("Test error {}", e))
            .ok();
        // An example on how to report to python manager
        // let mut report = ReportEntry::default();
        // report.flow_id = flow_id;
        // if let Some(ref mut s) = py_stream {
        //     let payload = report.as_bytes();
        //     let len = (payload.len() as u32).to_be_bytes();
        //     s.write_all(&len).unwrap();
        //     s.write_all(&payload).unwrap();
        // }
        Self {
            config,
            stats: Default::default(),
            init_time: Instant::now(),
            mode: CompetingMode::Default,
            slow_start: true,
            cwnd: initial_cwnd,
            velocity: Velocity::default(),
            delta: slow_start_delta,
            standing_rtt_filter: MinMax::new(STANDING_RTT_FILTER_WINDOW.as_micros() as u64),
            min_rtt_filter: MinMax::new(MIN_RTT_FILTER_WINDOW.as_micros() as u64),
            delivery_rate_estimator: DeliveryRateEstimator::default(),
            ack_state: Default::default(),
            increase_cwnd: false,
            target_rate: 0,
            last_sent_pkt_num: 0,
            round: Default::default(),
            connection_trace_id,
            flow_id,
            stream,
            shared_memory_handler: None,
            rate_residuals: VecDeque::new(),
            py_stream,
            rate_ewma: f64::MIN,
            rate_ewmv: 0.0,
            intervals_cnt: 0,
            last_update_ewma_time: Instant::now(),
            unsent_bytes: 0,
            recent_resp_time: Instant::now(),
            sending_large_resp: 0,
            sending_resp: 0,
            total_resp_cnts: 0,
            start_time: Instant::now(),
            min_delta: 0.05,
        }
    }

    fn update_ewma(&mut self) {
        // update ewma every round
        if self.delivery_rate_estimator.is_sample_app_limited() {
            log::info!("App limit!");
            return;
        }
        let mut rate_residual = 0.0;
        if self.rate_ewma < 0.0 {
            self.rate_ewma = self.delivery_rate_estimator.delivery_rate() as f64 / 1024.0 / 1024.0;
        } else {
            self.rate_ewma = 0.85 * self.rate_ewma
                + 0.15 * self.delivery_rate_estimator.delivery_rate() as f64 / 1024.0 / 1024.0;
            rate_residual = self.rate_ewma
                - self.delivery_rate_estimator.delivery_rate() as f64 / 1024.0 / 1024.0;
        }
        // update residuals
        if self.rate_residuals.len() >= 10 {
            self.rate_residuals.pop_front();
            self.rate_residuals.push_back(rate_residual);
        } else {
            self.rate_residuals.push_back(rate_residual);
        }
        // update ewmv
        let res_mean = self.rate_residuals.iter().sum::<f64>() / self.rate_residuals.len() as f64;
        self.rate_ewmv = self
            .rate_residuals
            .iter()
            .map(|&r| (r - res_mean) * (r - res_mean))
            .collect::<Vec<f64>>()
            .iter()
            .sum::<f64>()
            / self.rate_residuals.len() as f64;
        self.last_update_ewma_time = Instant::now();
        log::debug!(
            "END_ACK. rate_ewma = {}, rate_ewmv = {}",
            self.rate_ewma,
            self.rate_ewmv,
        );
    }

    /// Update velocity.
    // Once per window, the sender compares the current cwnd to the cwnd value at
    // the time that the latest acknowledged packet was sent (i.e., cwnd at the
    // start of the current window). If the current cwnd is larger, then set direction
    // to 'up'; if it is smaller, then set direction to 'down'. Now, if direction is
    // the same as in the previous window, then double v. If not, then reset v to 1.
    // However, start doubling v only after the direction has remained the same for three RTTs.
    fn update_velocity(&mut self) {
        // in the case that cwnd should increase in slow start, we do not need
        // to update velocity, since cwnd is always doubled.
        if self.slow_start && self.increase_cwnd {
            return;
        }

        // First time to run here.
        if self.velocity.last_cwnd == 0 {
            self.velocity.last_cwnd = self.cwnd.max(self.config.min_cwnd);
            self.velocity.velocity = 1;
            self.velocity.same_direction_cnt = 0;

            return;
        }

        // Update velocity at the beginning of each round.
        if !self.is_round_start() {
            return;
        }

        // Check cwnd growth direction.
        // if in slow start, and target rate is not reached, then increase cwnd anyway.
        // otherwise, check and update direction to determine cwnd growth in next steps.
        let new_direction = if self.cwnd > self.velocity.last_cwnd {
            Direction::Up
        } else {
            Direction::Down
        };

        // New interval, calc new delta
        if new_direction == Direction::Up && self.velocity.direction == Direction::Down {
            // self.update_delta();
            if USE_PREDICT {
                self.calc_delta_predic();
            } else {
                self.calc_delta_cur();
            }
            self.intervals_cnt += 1;
        }

        if new_direction != self.velocity.direction {
            // Direction changes, reset velocity.
            self.velocity.velocity = 1;
            self.velocity.same_direction_cnt = 0;
        } else {
            // Same direction, check to speed up.
            self.velocity.same_direction_cnt = self.velocity.same_direction_cnt.saturating_add(1);

            if self.velocity.same_direction_cnt >= SPEED_UP_THRESHOLD {
                self.velocity.velocity = self.velocity.velocity.saturating_mul(2);
            }
        }

        // if our current rate is much different than target, we double v every
        // RTT. That could result in a high v at some point in time. If we
        // detect a sudden direction change here, while v is still very high but
        // meant for opposite direction, we should reset it to 1.
        //
        // e.g. cwnd < last_recorded_cwnd && rate < target_rate
        // cwnd < last_recorded_cwnd means that direction is still DOWN while velocity may be large
        // rate < target_rate means that cwnd is about to increase
        // so a switch point is produced, we hope copa switch to increase up as soon as possible。
        if self.increase_cwnd
            && self.velocity.direction != Direction::Up
            && self.velocity.velocity > 1
        {
            self.velocity.direction = Direction::Up;
            self.velocity.velocity = 1;
        } else if !self.increase_cwnd
            && self.velocity.direction != Direction::Down
            && self.velocity.velocity > 1
        {
            self.velocity.direction = Direction::Down;
            self.velocity.velocity = 1;
        }

        self.velocity.direction = new_direction;
        self.velocity.last_cwnd = self.cwnd;
    }

    /// Update mode and parameter delta.
    fn update_mode(&mut self) {
        // Check if loss rate exceeds threshold when a new round starts. If so,
        // We assume that Copa should switch to competitive mode, to competing with
        // other buffer-filling flows.
        self.mode = if self.round.loss_rate >= LOSS_RATE_THRESHOLD {
            CompetingMode::Competitive
        } else {
            CompetingMode::Default
        };

        match self.mode {
            CompetingMode::Default => {
                self.delta = if self.slow_start {
                    self.config.slow_start_delta
                } else {
                    self.config.steady_delta
                };
            }
            CompetingMode::Competitive => {
                // Double delta to slow down the target rate.
                self.delta *= 2.0_f64;
                self.delta = self.delta.min(0.5);
            }
        }
    }

    /// Update congestion window.
    fn update_cwnd(&mut self) {
        // Deal with the following cases:
        // 1. slow_start, cwnd to increase: double cwnd
        // 2. slow_start, cwnd to decrease: exiting slow_start and decrease cwnd
        // 3. not slow_start, cwnd to increase: increase cwnd
        // 4. not slow_start, cwnd to decrease: decrease cwnd

        // Exit slow start once cwnd begins to decrease, i.e. rate reaches target rate.
        if self.slow_start && !self.increase_cwnd {
            self.slow_start = false;
        }

        if self.slow_start {
            // Stay in slow start until the target rate is reached.
            if self.increase_cwnd {
                self.cwnd = self.cwnd.saturating_add(self.ack_state.newly_acked_bytes);
            }
        } else {
            let cwnd_delta = ((self.velocity.velocity
                * self.ack_state.newly_acked_bytes
                * self.config.max_datagram_size) as f64
                / (self.delta * (self.cwnd as f64))) as u64;

            // Not in slow start. Adjust cwnd.
            self.cwnd = if self.increase_cwnd {
                self.cwnd.saturating_add(cwnd_delta)
            } else {
                self.cwnd.saturating_sub(cwnd_delta)
            };

            if self.cwnd == 0 {
                trace!("{}. cwnd is zero!!!", self.name());

                self.cwnd = self.config.min_cwnd;
                self.velocity.velocity = 1;
            }
        }
    }

    /// Check if a new round starts and update round.
    fn update_round(&mut self) {
        if self.ack_state.largest_acked_pkt_num >= self.round.round_end_pkt_num {
            // Calculate loss rate first and then update round states.
            let bytes_lost_in_this_round = self
                .stats
                .bytes_lost_in_total
                .saturating_sub(self.round.last_total_lost_bytes);
            let bytes_acked_in_this_round = self
                .stats
                .bytes_acked_in_total
                .saturating_sub(self.round.last_total_acked_bytes);

            self.round.loss_rate = bytes_lost_in_this_round as f64
                / bytes_lost_in_this_round.saturating_add(bytes_acked_in_this_round) as f64;

            self.round.last_total_acked_bytes = self.stats.bytes_acked_in_total;
            self.round.last_total_lost_bytes = self.stats.bytes_lost_in_total;
            self.round.round_count = self.round.round_count.saturating_add(1);
            self.round.round_end_pkt_num = self.last_sent_pkt_num;
            self.round.is_round_start = true;
            // self.update_delta();
        } else {
            self.round.is_round_start = false;
        }
    }

    /// Check if a new round starts.
    fn is_round_start(&self) -> bool {
        self.round.is_round_start
    }

    /// Update Copa model driven by ACK packet.
    fn update_model(&mut self) {
        // COPA algorithm processing steps:
        // 1. update d_q and srtt;
        // 2. set lambda_t to 1/(delta * d_q);
        // 3. adjust cwnd according to the relationship of lambda and lambda_t
        // 4. update velocity
        //
        // d_q = RTT_standing - minRTT, where RTT_standing is the minimum during time window srtt/2

        // Update standing rtt and min rtt.
        if self.config.use_standing_rtt {
            self.standing_rtt_filter
                .set_window(self.ack_state.last_srtt.as_micros() as u64);
        } else {
            self.standing_rtt_filter
                .set_window(self.ack_state.last_srtt.as_micros() as u64 / 2);
        }

        if self.ack_state.min_rtt == Duration::ZERO {
            self.ack_state.min_rtt = if self.ack_state.last_srtt == Duration::ZERO {
                self.config.initial_rtt.unwrap_or(Duration::from_millis(20))
            } else {
                self.ack_state.last_srtt
            };
        }

        // Update min rtt in period of standing window and 10s respectly.
        let elapsed = self.ack_state.now.saturating_duration_since(self.init_time);
        self.min_rtt_filter.update_min(
            elapsed.as_micros() as u64,
            self.ack_state.min_rtt.as_micros() as u64,
        );
        self.standing_rtt_filter.update_min(
            elapsed.as_micros() as u64,
            self.ack_state.min_rtt.as_micros() as u64,
        );

        // Adjust delta.
        self.update_mode();

        let min_rtt = Duration::from_micros(self.min_rtt_filter.get());
        let standing_rtt = self.get_standing_rtt();

        trace!(
            "{}. round_min_rtt = {}us, elapsed = {}us, min_rtt = {}us, standing_rtt = {}us",
            self.name(),
            self.ack_state.min_rtt.as_micros(),
            elapsed.as_micros(),
            min_rtt.as_micros(),
            standing_rtt.as_micros(),
        );

        let current_rate: u64 = (self.cwnd as f64 / standing_rtt.as_secs_f64()) as u64;
        let queueing_delay = standing_rtt.saturating_sub(min_rtt);
        if queueing_delay.is_zero() {
            // taking care of inf targetRate case here, this happens in beginning where
            // we do want to increase cwnd, e.g. slow start or no queuing happens.
            self.increase_cwnd = true;

            trace!(
                "{}. queuing delay is zero. rtt_standing and min_rtt is the same: {}us",
                self.name(),
                min_rtt.as_micros()
            );

            self.target_rate = (self.cwnd as f64 / standing_rtt.as_secs_f64()) as u64;
        } else {
            // Limit queueing_delay in case it's too small and get a huge target rate.
            self.target_rate = (self.config.max_datagram_size as f64
                / self.delta
                / queueing_delay.max(Duration::from_micros(1)).as_secs_f64())
                as u64;

            trace!(
                "{}. target_rate = {}, delta = {}, max_datagram_size = {}",
                self.name(),
                self.target_rate,
                self.delta,
                self.config.max_datagram_size
            );

            if USE_BOUNCE && self.delta < 0.2 && self.velocity.direction == Direction::Down {
                if self.intervals_cnt % BOUNCE_INTERVALS != 0 {
                    self.target_rate = (self.target_rate as f64 * 2.2) as u64;
                    log::info!("at time {} interval {} bounce ", Instant::now().duration_since(self.start_time).as_secs_f64(), self.intervals_cnt);
                }
            } else {
                log::info!("at time {} interval {} not bounce", Instant::now().duration_since(self.start_time).as_secs_f64(), self.intervals_cnt);
            }
            self.increase_cwnd = self.target_rate >= current_rate;
        }

        self.update_velocity();
        self.update_cwnd();

        trace!(
            "{}. mode = {:?}, slow_start={}, delta={}, target_rate={}, current_rate={},
             increase_cwnd={}, queuing_delay={}us, rtt_standing={}us, cwnd={}",
            self.name(),
            self.mode,
            self.slow_start,
            self.delta,
            self.target_rate,
            current_rate,
            self.increase_cwnd,
            queueing_delay.as_micros(),
            standing_rtt.as_micros(),
            self.cwnd
        );
    }

    /// Get standing rtt.
    fn get_standing_rtt(&self) -> Duration {
        let standing_rtt = Duration::from_micros(self.standing_rtt_filter.get());
        if standing_rtt.is_zero() {
            return std::cmp::max(
                self.config.initial_rtt.unwrap_or(crate::INITIAL_RTT),
                Duration::from_micros(1),
            );
        }
        standing_rtt
    }

    // 根据过往统计的resp大小和个数预测可能的新resp个数
    fn calc_delta_predic(&mut self) {
        if self.round.loss_rate > 0.06 {
            self.config.steady_delta =
                (self.config.steady_delta / (1.0 + self.round.loss_rate)).min(0.5_f64);
            self.min_delta = self.config.steady_delta;
            log::info!("In lossy state, update delta: {}", self.config.steady_delta);
            return;
        } else {
            self.min_delta = self.min_delta * 0.98;
            self.min_delta = self.min_delta.max(0.04);
        }

        if self.sending_resp >= 10 && self.sending_large_resp >= 4 {
            // no enough data
            let min_rtt = self.min_rtt_filter.get() as f64 / 1_000.0;
            let queue_delay_thr = min_rtt.min(40.0_f64);
            let probe_delta = 12.0 / queue_delay_thr / self.rate_ewma;
            self.config.steady_delta = self.min_delta.max(probe_delta).min(0.4);
            log::info!(
                "[CALC][DELTA] possible sender queue buffbloat, speed up with delta: {}",
                self.config.steady_delta
            );
            return;
        }


        // For the current unsent_bytes, we cannot optimize the small streams, and can only control the large streams to make way for potential future small streams.
        // Simply consider all current unsent_bytes as part of a single large stream.
        let min_rtt = self.min_rtt_filter.get() as f64 / 1_000.0;
        let mut opt_delta = self.delta;
        // in seconds
        let mut min_rct = f64::MAX;
        let avg_flows_per_sec: f64 = self.total_resp_cnts as f64
            / Instant::now().duration_since(self.start_time).as_secs_f64();
        let predic_finish_time: f64 = self.unsent_bytes as f64 / self.rate_ewma / 1024.0 / 1024.0;
        let predic_flows_to_come = avg_flows_per_sec * predic_finish_time;
        for delta in [0.05, 0.1, 0.2, 0.3, 0.4, 0.5] {
            if delta < self.min_delta {
                continue;
            }
            let cur_queue_delay = 1.2 / self.rate_ewma / delta;
            let rate_std = self.rate_ewmv.sqrt();
            // note: here we just simply use rate std to represent the BDP std
            let bdp_std = rate_std / 1.5 * min_rtt;
            let delta_tput = (1.0 / delta / (bdp_std * 2.0)).min(1.0_f64) * rate_std;
            let cur_rate = self.rate_ewma + self.delta;
            let cur_rct = self.unsent_bytes as f64 / cur_rate / 1024.0 / 1024.0
            + avg_flows_per_sec * cur_queue_delay / 1_000.0;
            log::info!(
                "[CALC][DELTA] avg_flows_ps: {} delta: {}, unsent: {}, cur_queue_delay: {}, rate_std: {}, bdp_std: {}, delta_tput: {}, cur_rate: {}, cur_rct: {}",
                avg_flows_per_sec, delta, self.unsent_bytes, cur_queue_delay, rate_std, bdp_std, delta_tput, cur_rate, cur_rct
            );
            if cur_rct < min_rct {
                opt_delta = delta;
                min_rct = cur_rct;
            }
        }
        let min_rtt = self.min_rtt_filter.get() as f64 / 1_000.0;
        let queue_delay_thr = (0.4 * min_rtt).min(20.0_f64);
        let max_delta = 12.0 / queue_delay_thr / self.rate_ewma;
        opt_delta = opt_delta.max(self.min_delta).min(max_delta);
        // Test
        self.config.steady_delta = self.min_delta.max(0.04);
        // self.config.steady_delta = opt_delta;
        log::info!(
            "In steady state, update delta: {}, min rtt: {}, min rct(theory): {}",
            opt_delta,
            min_rtt,
            min_rct
        );
    }

    // 根据目前的resp大小和个数直接计算
    fn calc_delta_cur(&mut self) {
        // if we are in high lossy state
        if self.round.loss_rate > 0.06 {
            self.config.steady_delta =
                (self.config.steady_delta / (1.0 - self.round.loss_rate)).min(0.5_f64);
            self.min_delta = self.config.steady_delta;
            log::info!("In lossy state, update delta: {}", self.config.steady_delta);
            return;
        } else {
            self.min_delta = self.min_delta * 0.9;
            self.min_delta = self.min_delta.max(0.05);
        }

        // minrtt in ms
        let min_rtt = self.min_rtt_filter.get() as f64 / 1_000.0;
        let mut max_qoe = f64::MIN;
        let mut opt_delta = self.config.steady_delta;
        let cur_rate_bytesps = self.rate_ewma * 1024.0 * 1024.0;
        let qoe_lambda = self.sending_resp as f64 / cur_rate_bytesps / cur_rate_bytesps;
        for delta in [0.04, 0.08, 0.1, 0.2, 0.3, 0.4, 0.5] {
            if delta < self.min_delta {
                continue;
            }
            let cur_queue_delay = 1.5 / self.rate_ewma / delta / 1000.0;
            let rate_std = self.rate_ewmv.sqrt();
            // note: here we just simply use rate std to represent the BDP std
            let bdp_std = rate_std / 1.5 * min_rtt;
            let delta_tput = (1.0 / delta / (bdp_std * 2.0)).min(1.0_f64) * rate_std;
            let cur_rate = self.rate_ewma + self.delta;
            let cur_qoe = cur_rate - qoe_lambda * cur_queue_delay;
            log::info!(
                "[CALC][DELTA] delta: {}, unsent: {}, cur_queue_delay: {}, rate_std: {}, bdp_std: {}, delta_tput: {}, cur_rate: {}, cur_qoe: {}",
                delta, self.unsent_bytes, cur_queue_delay, rate_std, bdp_std, delta_tput, cur_rate, cur_qoe
            );
            if max_qoe <= cur_qoe {
                opt_delta = delta;
                max_qoe = cur_qoe;
            }
        }
        self.config.steady_delta = opt_delta;
        log::info!("set delta: {}", self.config.steady_delta);
    }
}

impl CongestionController for Mortise {
    fn pacing_rate(&self) -> Option<u64> {
        let standing_rtt = self.get_standing_rtt();
        let current_rate = (self.cwnd as f64 / standing_rtt.as_secs_f64()) as u64;

        Some(PACING_GAIN * current_rate)
    }

    fn name(&self) -> &str {
        "COPA"
    }

    fn congestion_window(&self) -> u64 {
        self.cwnd.max(self.config.min_cwnd)
    }

    fn initial_window(&self) -> u64 {
        self.config.initial_cwnd
    }

    fn minimal_window(&self) -> u64 {
        self.config.min_cwnd
    }

    fn in_slow_start(&self) -> bool {
        self.slow_start
    }

    fn stats(&self) -> &CongestionStats {
        &self.stats
    }

    fn on_sent(&mut self, now: Instant, packet: &mut SentPacket, bytes_in_flight: u64) {
        self.stats.bytes_in_flight = bytes_in_flight;
        self.stats.bytes_sent_in_total = self
            .stats
            .bytes_sent_in_total
            .saturating_add(packet.sent_size as u64);

        self.last_sent_pkt_num = packet.pkt_num;

        self.delivery_rate_estimator.on_packet_sent(
            packet,
            self.stats.bytes_in_flight,
            self.stats.bytes_lost_in_total,
        );
    }

    fn begin_ack(&mut self, now: Instant, bytes_in_flight: u64) {
        self.ack_state.newly_acked_bytes = 0;
        self.ack_state.newly_lost_bytes = 0;
        self.ack_state.now = now;
        self.ack_state.min_rtt = Duration::ZERO;
        self.ack_state.last_srtt = Duration::ZERO;
        self.ack_state.largest_acked_pkt_num = 0;
    }

    fn on_ack(
        &mut self,
        packet: &mut SentPacket,
        now: Instant,
        _app_limited: bool,
        rtt: &RttEstimator,
        bytes_in_flight: u64,
    ) {
        // Update stats.
        // An example of how to get app_info
        // if let None = self.shared_memory_handler {
        //     self.shared_memory_handler = ShmemConf::new()
        //         .flink(format!("/tmp/mortise-quic-app-info-{}", self.flow_id))
        //         .open()
        //         .map_err(|e| log::error!("{}", e))
        //         .ok();
        // }
        // if let Some(ref mut h) = self.shared_memory_handler {
        //     unsafe {
        //         let app_info = QuicAppInfo::from_mut_bytes(h.as_slice_mut());
        //         if app_info.resp != 1 {
        //             debug!("get app info {}", app_info.req);
        //             self.unsent_bytes = app_info.req as u64;
        //             app_info.resp = 1;
        //         }
        //     }
        // }
        // Update rate sample by each ack packet.
        // log::info!("[OnAck] applimit {}", _app_limited);

        self.delivery_rate_estimator.update_rate_sample(packet);

        let sent_time = packet.time_sent;
        let acked_bytes = packet.sent_size as u64;

        self.stats.bytes_in_flight = self.stats.bytes_in_flight.saturating_sub(acked_bytes);
        self.stats.bytes_acked_in_total =
            self.stats.bytes_acked_in_total.saturating_add(acked_bytes);
        if self.in_slow_start() {
            self.stats.bytes_acked_in_slow_start = self
                .stats
                .bytes_acked_in_slow_start
                .saturating_add(acked_bytes);
        }

        self.ack_state.newly_acked_bytes =
            self.ack_state.newly_acked_bytes.saturating_add(acked_bytes);

        self.ack_state.largest_acked_pkt_num =
            self.ack_state.largest_acked_pkt_num.max(packet.pkt_num);
        self.ack_state.last_srtt = rtt.smoothed_rtt();

        // Since all the ack frames in a packet share the same ack_time and 'now_time',
        // we record only the minimum rtt and its corresponding time which will be
        // processed at the stage of end_ack.
        if self.ack_state.min_rtt.is_zero() || self.ack_state.min_rtt >= rtt.latest_rtt() {
            trace!(
                "{}. Got a smaller rtt: {}us -> {}us",
                self.name(),
                self.ack_state.min_rtt.as_micros(),
                rtt.latest_rtt().as_micros()
            );

            self.ack_state.min_rtt = rtt.latest_rtt();
        }

        trace!(
            "{}. ON_ACK. latest_rtt = {}us, srtt = {}us, newly_acked = {}, total_acked = {}",
            self.name(),
            rtt.latest_rtt().as_micros(),
            rtt.smoothed_rtt().as_micros(),
            self.ack_state.newly_acked_bytes,
            self.stats.bytes_acked_in_total,
        )
    }

    fn end_ack(&mut self) {
        // Generate rate sample.
        self.delivery_rate_estimator.generate_rate_sample();

        self.update_round();
        trace!(
            "{}. END_ACK. round_start = {:?}, round_count = {}, end_pkt = {}, loss_rate = {}, last_acked = {}, last_lost = {}, ewma = {}",
            self.name(),
            self.is_round_start(),
            self.round.round_count,
            self.round.round_end_pkt_num,
            self.round.loss_rate,
            self.round.last_total_acked_bytes,
            self.round.last_total_lost_bytes,
            self.rate_ewma,
        );

        self.update_model();

        // update every half rtt
        if Instant::now().duration_since(self.last_update_ewma_time)
            > Duration::from_micros(self.min_rtt_filter.get()).mul_f64(0.5)
        {
            self.update_ewma();
        }
    }

    fn on_congestion_event(
        &mut self,
        now: Instant,
        packet: &SentPacket,
        in_persistent_congestion: bool,
        lost_bytes: u64,
        bytes_in_flight: u64,
    ) {
        // Statistics.
        self.stats.bytes_lost_in_total = self.stats.bytes_lost_in_total.saturating_add(lost_bytes);
        self.stats.bytes_in_flight = bytes_in_flight;

        if self.in_slow_start() {
            self.stats.bytes_lost_in_slow_start = self
                .stats
                .bytes_lost_in_slow_start
                .saturating_add(lost_bytes);
        }
    }

    fn set_unsent_bytes(&mut self, bytes: u64) {
        self.unsent_bytes = bytes;
    }

    fn report_resp_size(&mut self, bytes: u64, state: bool) {
        // true: sending, false: send done
        if state {
            if bytes > 50_000 {
                self.sending_large_resp += 1;
            }
            self.total_resp_cnts += 1;
            self.sending_resp += 1;
            self.recent_resp_time = Instant::now();
        } else {
            if bytes > 50_000 {
                self.sending_large_resp -= 1;
            }
            self.sending_resp -= 1;
        }
    }
}
