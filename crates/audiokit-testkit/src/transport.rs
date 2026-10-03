//! Deterministic, bounded virtual packet delivery; no DSP or real network code.
use crate::{Error, Result};
use serde::{Deserialize, Serialize};

/// Explicit virtual-forwarder faults. Default is lossless, immediate delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TransportConfig {
    /// Deterministic SHA256 draw seed; seed zero is valid.
    pub seed: u64,
    /// Fixed one-way delivery delay, 0..=2000 ms.
    pub delay_ms: u16,
    /// Independent additional uniform delay, 0..=2000 ms (never negative).
    pub jitter_ms: u16,
    /// Independent drop probability per original packet, 0..=1000 per mille.
    pub loss_per_mille: u16,
    /// Chance of one extra copy of a surviving packet, 0..=1000 per mille.
    pub duplicate_per_mille: u16,
    /// Delay every Nth original if it survives; zero disables, one selects all.
    pub reorder_every: u16,
    /// Additional delay for the selected packet, 0..=2000 ms.
    pub reorder_delay_ms: u16,
    /// Start of a single forwarding pause on the virtual host clock, milliseconds.
    pub stall_start_ms: u32,
    /// Pause length, 0..=2000 ms; pending deliveries resume at its end.
    pub stall_duration_ms: u16,
    /// Maximum pending payload copies, 1..=4096; overflow fails explicitly.
    pub max_pending_packets: usize,
}
impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            seed: 0,
            delay_ms: 0,
            jitter_ms: 0,
            loss_per_mille: 0,
            duplicate_per_mille: 0,
            reorder_every: 0,
            reorder_delay_ms: 0,
            stall_start_ms: 0,
            stall_duration_ms: 0,
            max_pending_packets: 1024,
        }
    }
}
impl TransportConfig {
    /// Validates fault magnitudes and queue limits without allocating payloads.
    pub fn validate(&self) -> Result<()> {
        if self.delay_ms > 2000
            || self.jitter_ms > 2000
            || self.reorder_delay_ms > 2000
            || self.stall_duration_ms > 2000
            || self.loss_per_mille > 1000
            || self.duplicate_per_mille > 1000
            || !(1..=4096).contains(&self.max_pending_packets)
            || ((self.reorder_every == 0) != (self.reorder_delay_ms == 0))
            || (self.stall_duration_ms == 0 && self.stall_start_ms != 0)
        {
            return Err(Error::Invalid(
                "invalid virtual transport faults or budget".into(),
            ));
        }
        Ok(())
    }
    /// True when delivery timing or packet multiplicity can differ from the clean baseline.
    pub fn is_impaired(&self) -> bool {
        self.delay_ms != 0
            || self.jitter_ms != 0
            || self.loss_per_mille != 0
            || self.duplicate_per_mille != 0
            || self.reorder_every != 0
            || self.stall_duration_ms != 0
    }
}

#[cfg(feature = "codec-opus")]
pub(crate) mod scheduler {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::collections::BTreeMap;

    #[derive(Debug, Default, Serialize)]
    pub(crate) struct Stats {
        pub original_packets: u64,
        pub intentionally_dropped: u64,
        pub duplicated_packets: u64,
        pub delivered_copies: u64,
        pub pending_copies: usize,
        pub max_pending_copies: usize,
        pub reorder_selected_packets: u64,
        pub stalled_packets: u64,
        pub max_delivery_delay_ns: u64,
    }
    #[derive(Debug, Serialize)]
    pub(crate) struct Decision {
        pub packet_ordinal: u64,
        pub sequence: u16,
        pub dropped: bool,
        pub copies: u8,
        pub emitted_ns: u64,
        pub due_ns: Option<u64>,
        pub reorder_selected: bool,
        pub stalled: bool,
    }
    pub(crate) struct Delivery {
        pub ordinal: u64,
        pub sequence: u16,
        pub first_frame: u64,
        pub emitted_ns: u64,
        pub due_ns: u64,
        pub duplicate: bool,
        pub payload: Vec<u8>,
    }
    pub(crate) struct Transport {
        config: TransportConfig,
        pending: BTreeMap<(u64, u64, u8), Delivery>,
        stats: Stats,
    }
    impl Transport {
        pub fn new(config: TransportConfig) -> Self {
            Self {
                config,
                pending: BTreeMap::new(),
                stats: Stats::default(),
            }
        }
        // Domain-separated SHA256 draws are stable across platforms/crate versions.
        // Modulo bias is negligible here; this is fault selection, not cryptography.
        fn draw(&self, ordinal: u64, domain: u8, range: u64) -> u64 {
            let mut hash = Sha256::new();
            hash.update(self.config.seed.to_le_bytes());
            hash.update(ordinal.to_le_bytes());
            hash.update([domain]);
            let bytes = hash.finalize();
            u64::from_le_bytes(bytes[..8].try_into().expect("SHA256 prefix")) % range
        }
        pub fn schedule(
            &mut self,
            payload: Vec<u8>,
            sequence: u16,
            first_frame: u64,
            now: u64,
        ) -> Result<Decision> {
            let ordinal = self.stats.original_packets;
            let dropped = self.draw(ordinal, 0, 1000) < u64::from(self.config.loss_per_mille);
            let duplicate = !dropped
                && self.draw(ordinal, 1, 1000) < u64::from(self.config.duplicate_per_mille);
            let copies = if dropped {
                0
            } else if duplicate {
                2
            } else {
                1
            };
            if self.pending.len() + usize::from(copies) > self.config.max_pending_packets {
                return Err(Error::Execution(
                    "virtual transport pending packet budget exhausted".into(),
                ));
            }
            let reorder = !dropped
                && self.config.reorder_every != 0
                && (ordinal + 1).is_multiple_of(u64::from(self.config.reorder_every));
            let delay_ms = u64::from(self.config.delay_ms)
                + self.draw(ordinal, 2, u64::from(self.config.jitter_ms) + 1)
                + if reorder {
                    u64::from(self.config.reorder_delay_ms)
                } else {
                    0
                };
            let mut due = now + delay_ms * 1_000_000;
            let stall_start = u64::from(self.config.stall_start_ms) * 1_000_000;
            let stall_end = stall_start + u64::from(self.config.stall_duration_ms) * 1_000_000;
            let stalled = !dropped && (stall_start..stall_end).contains(&due);
            if stalled {
                due = stall_end;
            }
            self.stats.original_packets += 1;
            if dropped {
                self.stats.intentionally_dropped += 1;
            } else {
                if duplicate {
                    self.stats.duplicated_packets += 1;
                }
                self.stats.reorder_selected_packets += u64::from(reorder);
                self.stats.stalled_packets += u64::from(stalled);
                self.stats.max_delivery_delay_ns = self.stats.max_delivery_delay_ns.max(due - now);
                if duplicate {
                    self.pending.insert(
                        (due, ordinal, 1),
                        Delivery {
                            ordinal,
                            sequence,
                            first_frame,
                            emitted_ns: now,
                            due_ns: due,
                            duplicate: true,
                            payload: payload.clone(),
                        },
                    );
                }
                self.pending.insert(
                    (due, ordinal, 0),
                    Delivery {
                        ordinal,
                        sequence,
                        first_frame,
                        emitted_ns: now,
                        due_ns: due,
                        duplicate: false,
                        payload,
                    },
                );
                self.stats.pending_copies = self.pending.len();
                self.stats.max_pending_copies =
                    self.stats.max_pending_copies.max(self.pending.len());
            }
            Ok(Decision {
                packet_ordinal: ordinal,
                sequence,
                dropped,
                copies,
                emitted_ns: now,
                due_ns: if dropped { None } else { Some(due) },
                reorder_selected: reorder,
                stalled,
            })
        }
        pub fn pop_due(&mut self, now: u64) -> Option<Delivery> {
            if self
                .pending
                .first_key_value()
                .is_none_or(|(key, _)| key.0 > now)
            {
                return None;
            }
            let (_, delivery) = self.pending.pop_first()?;
            self.stats.delivered_copies += 1;
            self.stats.pending_copies = self.pending.len();
            Some(delivery)
        }
        pub fn is_empty(&self) -> bool {
            self.pending.is_empty()
        }
        pub fn statistics(&self) -> &Stats {
            &self.stats
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn duplicates_keep_stable_order_and_budget_is_checked_before_mutation() {
            let mut transport = Transport::new(TransportConfig {
                duplicate_per_mille: 1000,
                max_pending_packets: 2,
                ..Default::default()
            });
            transport.schedule(vec![1], 65535, 0, 10).unwrap();
            assert!(transport.schedule(vec![2], 0, 960, 10).is_err());
            assert_eq!(transport.statistics().original_packets, 1);
            let first = transport.pop_due(10).unwrap();
            let second = transport.pop_due(10).unwrap();
            assert!(!first.duplicate);
            assert!(second.duplicate);
            assert_eq!(first.payload, second.payload);
            assert!(transport.is_empty());
        }
        #[test]
        fn fixed_seed_decisions_are_reproducible_and_loss_is_not_queued() {
            let config = TransportConfig {
                seed: 17,
                jitter_ms: 30,
                loss_per_mille: 450,
                ..Default::default()
            };
            let sample = |seed_config| {
                let mut transport = Transport::new(seed_config);
                (0..100)
                    .map(|i| {
                        serde_json::to_value(
                            transport
                                .schedule(vec![0], i, u64::from(i) * 960, u64::from(i) * 20_000_000)
                                .unwrap(),
                        )
                        .unwrap()
                    })
                    .collect::<Vec<_>>()
            };
            assert_eq!(sample(config), sample(config));
            assert_ne!(
                sample(config),
                sample(TransportConfig { seed: 18, ..config })
            );
            let mut lost = Transport::new(TransportConfig {
                loss_per_mille: 1000,
                ..Default::default()
            });
            assert!(lost.schedule(vec![0], 0, 0, 0).unwrap().dropped);
            assert!(lost.is_empty());
        }
        #[test]
        fn reorder_and_stall_change_forwarding_not_packet_identity() {
            let mut transport = Transport::new(TransportConfig {
                reorder_every: 2,
                reorder_delay_ms: 60,
                stall_start_ms: 50,
                stall_duration_ms: 100,
                ..Default::default()
            });
            transport.schedule(vec![0], 0, 0, 20_000_000).unwrap();
            transport.schedule(vec![1], 1, 960, 40_000_000).unwrap();
            transport.schedule(vec![2], 2, 1920, 60_000_000).unwrap();
            assert_eq!(transport.pop_due(20_000_000).unwrap().sequence, 0);
            assert!(transport.pop_due(149_000_000).is_none());
            assert_eq!(transport.pop_due(150_000_000).unwrap().sequence, 1);
            assert_eq!(transport.pop_due(150_000_000).unwrap().sequence, 2);
            assert_eq!(transport.statistics().stalled_packets, 2);
        }
    }
}
