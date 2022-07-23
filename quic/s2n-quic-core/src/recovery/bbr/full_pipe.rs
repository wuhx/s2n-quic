// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    counter::{Counter, Saturating},
    recovery::{bandwidth, bandwidth::Bandwidth, bbr::BbrCongestionController},
};
use num_rational::Ratio;

/// Estimator for determining if BBR has fully utilized its available bandwidth ("filled the pipe")
#[derive(Debug, Default, Clone)]
pub(crate) struct Estimator {
    //= https://tools.ietf.org/id/draft-cardwell-iccrg-bbr-congestion-control-02#4.3.1.2
    //# BBRInitFullPipe():
    //#  BBR.filled_pipe = false
    //#  BBR.full_bw = 0
    //#  BBR.full_bw_count = 0

    //= https://tools.ietf.org/id/draft-cardwell-iccrg-bbr-congestion-control-02#2.13
    //# A boolean that records whether BBR estimates that it has ever
    //# fully utilized its available bandwidth ("filled the pipe").
    filled_pipe: bool,
    //= https://tools.ietf.org/id/draft-cardwell-iccrg-bbr-congestion-control-02#2.13
    //# A recent baseline BBR.max_bw to estimate if BBR has "filled the pipe" in Startup.
    full_bw: Bandwidth,
    //= https://tools.ietf.org/id/draft-cardwell-iccrg-bbr-congestion-control-02#2.13
    //# The number of non-app-limited round trips without large increases in BBR.full_bw.
    full_bw_count: Counter<u8, Saturating>,
    /// The number of discontiguous bursts of lost packets in the last round
    loss_bursts: Counter<u8, Saturating>,
    /// The number of rounds where the ECN CE markings exceed ECN_THRESH
    ecn_ce_rounds: Counter<u8, Saturating>,
    /// True if BBR was in fast recovery in the last round
    in_recovery_last_round: bool,
}

impl Estimator {
    /// Returns true if BBR estimates that is has ever fully utilized its available bandwidth
    #[inline]
    pub fn filled_pipe(&self) -> bool {
        self.filled_pipe
    }

    /// Called on each new BBR round
    #[inline]
    pub fn on_round_start(
        &mut self,
        rate_sample: bandwidth::RateSample,
        max_bw: Bandwidth,
        in_recovery: bool,
        ecn_ce_count_too_high: bool,
        max_datagram_size: u16,
    ) {
        if self.filled_pipe {
            return;
        }

        self.filled_pipe = self.bandwidth_plateaued(rate_sample, max_bw)
            || self.excessive_inflight(rate_sample, in_recovery, max_datagram_size)
            || self.excessive_explicit_congestion(ecn_ce_count_too_high);
    }

    /// Determines if the rate of increase of bandwidth has decreased enough to estimate the
    /// available bandwidth has been fully utilized.
    ///
    /// Based on bbr_check_full_bw_reached in tcp_bbr2.c
    #[inline]
    fn bandwidth_plateaued(
        &mut self,
        rate_sample: bandwidth::RateSample,
        max_bw: Bandwidth,
    ) -> bool {
        //= https://tools.ietf.org/id/draft-cardwell-iccrg-bbr-congestion-control-02#4.3.1.2
        //# BBRCheckStartupFullBandwidth():
        //#   if BBR.filled_pipe or
        //#     !BBR.round_start or rs.is_app_limited
        //#    return  /* no need to check for a full pipe now */
        //#   if (BBR.max_bw >= BBR.full_bw * 1.25)  /* still growing? */
        //#     BBR.full_bw = BBR.max_bw    /* record new baseline level */
        //#     BBR.full_bw_count = 0
        //#   return
        //#   BBR.full_bw_count++ /* another round w/o much growth */
        //#   if (BBR.full_bw_count >= 3)
        //#     BBR.filled_pipe = true

        //# If BBR notices that there are several (three) rounds where attempts to double
        //# the delivery rate actually result in little increase (less than 25 percent),
        //# then it estimates that it has reached BBR.max_bw, sets BBR.filled_pipe to true,
        //# exits Startup and enters Drain.
        const DELIVERY_RATE_INCREASE: Ratio<u64> = Ratio::new_raw(4, 3); // 1.25
        const BANDWIDTH_PLATEAU_ROUND_COUNT: u8 = 3;

        if rate_sample.is_app_limited {
            //= https://tools.ietf.org/id/draft-cardwell-iccrg-bbr-congestion-control-02#4.3.1.2
            //# Once per round trip, upon an ACK that acknowledges new data, and when
            //# the delivery rate sample is not application-limited (see [draft-
            //# cheng-iccrg-delivery-rate-estimation]), BBR runs the "full pipe" estimator
            return false;
        }

        if max_bw >= self.full_bw * DELIVERY_RATE_INCREASE {
            // still growing?
            self.full_bw = max_bw; // record new baseline level
            self.full_bw_count = Counter::default(); // restart the count
            return false;
        }

        /* another round w/o much growth */
        self.full_bw_count += 1;

        // Bandwidth has plateaued if the number of rounds without much growth
        // reaches `BANDWIDTH_PLATEAU_ROUND_COUNT`
        self.full_bw_count >= BANDWIDTH_PLATEAU_ROUND_COUNT
    }

    /// Determines if inflight has been too high (due to either loss or ECN markings) for enough
    /// round trips to estimate the available bandwidth has been fully utilized.
    #[inline]
    fn excessive_inflight(
        &mut self,
        rate_sample: bandwidth::RateSample,
        in_recovery: bool,
        max_datagram_size: u16,
    ) -> bool {
        //= https://tools.ietf.org/id/draft-cardwell-iccrg-bbr-congestion-control-02#4.3.1.3
        //# A second method BBR uses for estimating the bottleneck is full is by looking at sustained
        //# packet losses Specifically for a case where the following criteria are all met:
        //#
        //#    *  The connection has been in fast recovery for at least one full round trip.
        //#    *  The loss rate over the time scale of a single full round trip exceeds BBRLossThresh (2%).
        //#    *  There are at least BBRStartupFullLossCnt=3 discontiguous sequence ranges lost in that round trip.
        const STARTUP_FULL_LOSS_COUNT: u8 = 3;

        if in_recovery
            && self.in_recovery_last_round
            && BbrCongestionController::is_inflight_too_high(rate_sample, max_datagram_size)
            && self.loss_bursts >= STARTUP_FULL_LOSS_COUNT
        {
            return true;
        }

        self.in_recovery_last_round = in_recovery;
        self.loss_bursts = Counter::default();

        false
    }

    /// Determines if enough consecutive rounds of explicit congestion have been encountered that we
    /// can estimate the available bandwidth has been fully utilized.
    ///
    /// Based on bbr2_check_ecn_too_high_in_startup from https://github.com/google/bbr/blob/1a45fd4faf30229a3d3116de7bfe9d2f933d3562/net/ipv4/tcp_bbr2.c#L1372
    fn excessive_explicit_congestion(&mut self, ecn_ce_count_too_high: bool) -> bool {
        // Startup is exited if the number of consecutive round trips with ECN CE markings above
        // the ECN_THRESH exceed this value
        // Value from https://github.com/google/bbr/blob/1a45fd4faf30229a3d3116de7bfe9d2f933d3562/net/ipv4/tcp_bbr2.c#L2334
        const STARTUP_FULL_ECN_COUNT: u8 = 2;

        if ecn_ce_count_too_high {
            self.ecn_ce_rounds += 1;
        } else {
            self.ecn_ce_rounds = Counter::default();
        }

        self.ecn_ce_rounds >= STARTUP_FULL_ECN_COUNT
    }

    /// Called for each lost packet
    #[inline]
    pub fn on_packet_lost(&mut self, new_loss_burst: bool) {
        if self.filled_pipe {
            return;
        }

        if new_loss_burst {
            self.loss_bursts += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        path::MINIMUM_MTU,
        recovery::{bandwidth::RateSample, bbr::full_pipe},
    };
    use core::time::Duration;

    #[test]
    fn bandwidth_plateau() {
        let mut fp_estimator = full_pipe::Estimator::default();
        let rate_sample = RateSample::default();
        let mut max_bw = Bandwidth::new(1000, Duration::from_secs(1));
        fp_estimator.on_round_start(rate_sample, max_bw, false, false, MINIMUM_MTU);

        // Grow at 25% over 3 rounds
        max_bw = max_bw * Ratio::new(4, 3); // 4/3 = 125%
        for _ in 0..3 {
            fp_estimator.on_round_start(rate_sample, max_bw, false, false, MINIMUM_MTU);
        }
        // The pipe has not been filled yet since we have continued to grow bandwidth
        assert!(!fp_estimator.filled_pipe());

        // One more round with 24% growth, not growing fast enough to continue
        max_bw = max_bw * Ratio::new(31, 25); // 31/25 = 124%
        fp_estimator.on_round_start(rate_sample, max_bw, false, false, MINIMUM_MTU);
        // The pipe is considered full
        assert!(fp_estimator.filled_pipe());
    }

    #[test]
    fn bandwidth_plateau_app_limited() {
        let mut fp_estimator = full_pipe::Estimator::default();
        let rate_sample = RateSample {
            is_app_limited: true,
            ..Default::default()
        };
        let max_bw = Bandwidth::new(1000, Duration::from_secs(1));

        // No growth, but app limited
        for _ in 0..3 {
            fp_estimator.on_round_start(rate_sample, max_bw, false, false, MINIMUM_MTU);
        }

        // The pipe has not been filled yet since we were app limited
        assert!(!fp_estimator.filled_pipe());
    }

    #[test]
    fn excessive_inflight_due_to_loss() {
        let mut fp_estimator = full_pipe::Estimator::default();
        let rate_sample = RateSample {
            // Set app_limited to true to ignore bandwidth plateau check
            is_app_limited: true,
            // More than 2% bytes lost
            bytes_in_flight: 1000,
            lost_bytes: 21,
            ..Default::default()
        };
        let max_bw = Bandwidth::new(1000, Duration::from_secs(1));

        // In recovery the first round
        fp_estimator.on_round_start(rate_sample, max_bw, true, false, MINIMUM_MTU);

        // Only 2 loss bursts, not enough to be considered excessive loss
        fp_estimator.on_packet_lost(true);
        fp_estimator.on_packet_lost(true);

        fp_estimator.on_round_start(rate_sample, max_bw, true, false, MINIMUM_MTU);
        // The pipe has not been filled yet since there were only 2 loss bursts
        assert!(!fp_estimator.filled_pipe());

        // 3 loss bursts, enough to be considered excessive loss
        fp_estimator.on_packet_lost(true);
        fp_estimator.on_packet_lost(true);
        fp_estimator.on_packet_lost(true);

        fp_estimator.on_round_start(rate_sample, max_bw, true, false, MINIMUM_MTU);
        // The pipe has been filled due to loss
        assert!(fp_estimator.filled_pipe());
    }

    #[test]
    fn excessive_inflight_due_to_ecn_ce() {
        let mut fp_estimator = full_pipe::Estimator::default();
        let rate_sample = RateSample {
            // Set app_limited to true to ignore bandwidth plateau check
            is_app_limited: true,
            // >= ECN_THRESH (50%) of packets had ECN CE markings
            ecn_ce_count: 5,
            delivered_bytes: 9 * MINIMUM_MTU as u64,
            ..Default::default()
        };
        let max_bw = Bandwidth::new(1000, Duration::from_secs(1));

        // In recovery the first round
        fp_estimator.on_round_start(rate_sample, max_bw, true, false, MINIMUM_MTU);

        // Only 2 loss bursts, not enough to be considered excessive loss
        fp_estimator.on_packet_lost(true);
        fp_estimator.on_packet_lost(true);

        fp_estimator.on_round_start(rate_sample, max_bw, true, false, MINIMUM_MTU);
        // The pipe has not been filled yet since there were only 2 loss bursts
        assert!(!fp_estimator.filled_pipe());

        // 3 loss bursts, enough to be considered excessive loss
        fp_estimator.on_packet_lost(true);
        fp_estimator.on_packet_lost(true);
        fp_estimator.on_packet_lost(true);

        fp_estimator.on_round_start(rate_sample, max_bw, true, false, MINIMUM_MTU);
        // The pipe has been filled due to ECN
        assert!(fp_estimator.filled_pipe());
    }

    #[test]
    fn excessive_inflight_loss_rate_too_low() {
        let mut fp_estimator = full_pipe::Estimator::default();
        let rate_sample = RateSample {
            // Set app_limited to true to ignore bandwidth plateau check
            is_app_limited: true,
            // 2% bytes lost, just below the threshold to be considered excessive
            bytes_in_flight: 1000,
            lost_bytes: 2,
            ..Default::default()
        };
        let max_bw = Bandwidth::new(1000, Duration::from_secs(1));

        // In recovery the first round
        fp_estimator.on_round_start(rate_sample, max_bw, true, false, MINIMUM_MTU);

        // 3 loss bursts, enough to be considered excessive loss
        fp_estimator.on_packet_lost(true);
        fp_estimator.on_packet_lost(true);
        fp_estimator.on_packet_lost(true);

        fp_estimator.on_round_start(rate_sample, max_bw, true, false, MINIMUM_MTU);
        // The pipe has not been filled yet since the loss rate was not high enough
        assert!(!fp_estimator.filled_pipe());
    }

    #[test]
    fn excessive_inflight_not_in_recovery_long_enough() {
        let mut fp_estimator = full_pipe::Estimator::default();
        let rate_sample = RateSample {
            // Set app_limited to true to ignore bandwidth plateau check
            is_app_limited: true,
            // More than 2% bytes lost
            bytes_in_flight: 1000,
            lost_bytes: 21,
            ..Default::default()
        };
        let max_bw = Bandwidth::new(1000, Duration::from_secs(1));

        // Not in recovery the first round
        fp_estimator.on_round_start(rate_sample, max_bw, false, false, MINIMUM_MTU);

        // 3 loss bursts, enough to be considered excessive loss
        fp_estimator.on_packet_lost(true);
        fp_estimator.on_packet_lost(true);
        fp_estimator.on_packet_lost(true);

        fp_estimator.on_round_start(rate_sample, max_bw, true, false, MINIMUM_MTU);
        // The pipe has not been filled yet since we haven't been in recovery for a full round
        assert!(!fp_estimator.filled_pipe());
    }

    #[test]
    fn excessive_explicit_congestion() {
        let mut fp_estimator = full_pipe::Estimator::default();
        let rate_sample = RateSample {
            // Set app_limited to true to ignore bandwidth plateau check
            is_app_limited: true,
            ..Default::default()
        };

        let max_bw = Bandwidth::new(1000, Duration::from_secs(1));

        fp_estimator.on_round_start(rate_sample, max_bw, false, true, MINIMUM_MTU);
        // The pipe has not been filled yet since there was only one round with high ECN CE markings
        assert!(!fp_estimator.filled_pipe());

        fp_estimator.on_round_start(rate_sample, max_bw, false, false, MINIMUM_MTU);
        fp_estimator.on_round_start(rate_sample, max_bw, false, true, MINIMUM_MTU);
        // The pipe has not been filled yet since the low ecn rate sample reset the count,
        // ie the high ecn rate samples were not contiguous
        assert!(!fp_estimator.filled_pipe());

        fp_estimator.on_round_start(rate_sample, max_bw, false, true, MINIMUM_MTU);
        // After two consecutive rounds of high ECN markings, the pipe is full
        assert!(fp_estimator.filled_pipe());
    }
}
