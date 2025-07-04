// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Types for commands to clusters.

use std::num::NonZeroI64;
use std::str::FromStr;

use mz_proto::{ProtoType, RustType, TryFromProtoError};
use proptest::prelude::{Arbitrary, any};
use proptest::strategy::{BoxedStrategy, Strategy};
use proptest_derive::Arbitrary;
use serde::{Deserialize, Serialize};

include!(concat!(env!("OUT_DIR"), "/mz_cluster_client.client.rs"));

/// A value generated by environmentd and passed to the clusterd processes
/// to help them disambiguate different `CreateTimely` commands.
///
/// The semantics of this value are not important, except that they
/// must be totally ordered, and any value (for a given replica) must
/// be greater than any that were generated before (for that replica).
/// This is the reason for having two
/// components (one from the catalog storage that increases on every environmentd restart,
/// another in-memory and local to the current incarnation of environmentd)
#[derive(PartialEq, Eq, Debug, Copy, Clone, Serialize, Deserialize)]
pub struct ClusterStartupEpoch {
    /// The environment incarnation.
    envd: NonZeroI64,
    /// The replica incarnation.
    replica: u64,
}

impl ClusterStartupEpoch {
    /// Increases the replica incarnation counter.
    pub fn bump_replica(&mut self) {
        self.replica += 1;
    }
}

impl RustType<ProtoClusterStartupEpoch> for ClusterStartupEpoch {
    fn into_proto(&self) -> ProtoClusterStartupEpoch {
        let Self { envd, replica } = self;
        ProtoClusterStartupEpoch {
            envd: envd.get(),
            replica: *replica,
        }
    }

    fn from_proto(proto: ProtoClusterStartupEpoch) -> Result<Self, TryFromProtoError> {
        let ProtoClusterStartupEpoch { envd, replica } = proto;
        Ok(Self {
            envd: envd.try_into().unwrap(),
            replica,
        })
    }
}

impl Arbitrary for ClusterStartupEpoch {
    type Strategy = BoxedStrategy<Self>;
    type Parameters = ();

    fn arbitrary_with(_: Self::Parameters) -> Self::Strategy {
        (any::<i64>(), any::<u64>())
            .prop_map(|(envd, replica)| ClusterStartupEpoch {
                envd: NonZeroI64::new(if envd == 0 { envd + 1 } else { envd }).unwrap(),
                replica,
            })
            .boxed()
    }
}

impl ClusterStartupEpoch {
    /// Construct a new cluster startup epoch, from the environment epoch and replica incarnation.
    pub fn new(envd: NonZeroI64, replica: u64) -> Self {
        Self { envd, replica }
    }

    /// Serialize for transfer over the network
    pub fn to_bytes(&self) -> [u8; 16] {
        let mut ret = [0; 16];
        let mut p = &mut ret[..];
        use std::io::Write;
        p.write_all(&self.envd.get().to_be_bytes()[..]).unwrap();
        p.write_all(&self.replica.to_be_bytes()[..]).unwrap();
        ret
    }

    /// Inverse of `to_bytes`
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        let envd = i64::from_be_bytes((&bytes[0..8]).try_into().unwrap());
        let replica = u64::from_be_bytes((&bytes[8..16]).try_into().unwrap());
        Self {
            envd: envd.try_into().unwrap(),
            replica,
        }
    }

    /// The environment epoch.
    pub fn envd(&self) -> NonZeroI64 {
        self.envd
    }

    /// The replica incarnation.
    pub fn replica(&self) -> u64 {
        self.replica
    }
}

impl std::fmt::Display for ClusterStartupEpoch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self { envd, replica } = self;
        write!(f, "({envd}, {replica})")
    }
}

impl PartialOrd for ClusterStartupEpoch {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ClusterStartupEpoch {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        let Self { envd, replica } = self;
        let Self {
            envd: other_envd,
            replica: other_replica,
        } = other;
        (envd, replica).cmp(&(other_envd, other_replica))
    }
}

/// Configuration of the cluster we will spin up
#[derive(Arbitrary, Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TimelyConfig {
    /// Number of per-process worker threads
    pub workers: usize,
    /// Identity of this process
    pub process: usize,
    /// Addresses of all processes
    pub addresses: Vec<String>,
    /// Proportionality value that decides whether to exert additional arrangement merge effort.
    ///
    /// Specifically, additional merge effort is exerted when the size of the second-largest batch
    /// in an arrangement is within a factor of `arrangement_exert_proportionality` of the size of
    /// the largest batch, or when a merge is already in progress.
    ///
    /// The higher the proportionality value, the more eagerly arrangement batches are merged. A
    /// value of `0` (or `1`) disables eager merging.
    pub arrangement_exert_proportionality: u32,
    /// Whether to use the zero copy allocator.
    pub enable_zero_copy: bool,
    /// Whether to use lgalloc to back the zero copy allocator.
    pub enable_zero_copy_lgalloc: bool,
    /// Optional limit on the number of empty buffers retained by the zero copy allocator.
    pub zero_copy_limit: Option<usize>,
}

impl ToString for TimelyConfig {
    fn to_string(&self) -> String {
        serde_json::to_string(self).unwrap()
    }
}

impl FromStr for TimelyConfig {
    type Err = serde_json::Error;

    fn from_str(s: &str) -> serde_json::Result<Self> {
        serde_json::from_str(s)
    }
}

impl RustType<ProtoTimelyConfig> for TimelyConfig {
    fn into_proto(&self) -> ProtoTimelyConfig {
        ProtoTimelyConfig {
            workers: self.workers.into_proto(),
            addresses: self.addresses.into_proto(),
            process: self.process.into_proto(),
            arrangement_exert_proportionality: self.arrangement_exert_proportionality,
            enable_zero_copy: self.enable_zero_copy,
            enable_zero_copy_lgalloc: self.enable_zero_copy_lgalloc,
            zero_copy_limit: self.zero_copy_limit.into_proto(),
        }
    }

    fn from_proto(proto: ProtoTimelyConfig) -> Result<Self, TryFromProtoError> {
        Ok(Self {
            process: proto.process.into_rust()?,
            workers: proto.workers.into_rust()?,
            addresses: proto.addresses.into_rust()?,
            arrangement_exert_proportionality: proto.arrangement_exert_proportionality,
            enable_zero_copy: proto.enable_zero_copy,
            enable_zero_copy_lgalloc: proto.enable_zero_copy_lgalloc,
            zero_copy_limit: proto.zero_copy_limit.into_rust()?,
        })
    }
}

impl TimelyConfig {
    /// Split the timely configuration into `parts` pieces, each with a different `process` number.
    pub fn split_command(&self, parts: usize) -> Vec<Self> {
        (0..parts)
            .map(|part| TimelyConfig {
                process: part,
                ..self.clone()
            })
            .collect()
    }
}

/// A trait for specific cluster commands that can be unpacked into
/// `CreateTimely` variants.
pub trait TryIntoTimelyConfig {
    /// Attempt to unpack `self` into a `(TimelyConfig, ClusterStartupEpoch)`. Otherwise,
    /// fail and return `self` back.
    fn try_into_timely_config(self) -> Result<(TimelyConfig, ClusterStartupEpoch), Self>
    where
        Self: Sized;
}

/// Specifies the location of a cluster replica.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClusterReplicaLocation {
    /// The network addresses of the cluster control endpoints for each process in
    /// the replica. Connections from the controller to these addresses
    /// are sent commands, and send responses back.
    pub ctl_addrs: Vec<String>,
    /// The network addresses of the dataflow (Timely) endpoints for
    /// each process in the replica. These are used for _internal_
    /// networking, that is, timely worker communicating messages
    /// between themselves.
    pub dataflow_addrs: Vec<String>,
    /// The workers per process in the replica.
    pub workers: usize,
}

#[cfg(test)]
mod tests {
    use mz_ore::assert_ok;
    use mz_proto::protobuf_roundtrip;
    use proptest::prelude::ProptestConfig;
    use proptest::proptest;

    use super::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(32))]

        #[mz_ore::test]
        #[cfg_attr(miri, ignore)] // slow
        fn timely_config_protobuf_roundtrip(expect in any::<TimelyConfig>() ) {
            let actual = protobuf_roundtrip::<_, ProtoTimelyConfig>(&expect);
            assert_ok!(actual);
            assert_eq!(actual.unwrap(), expect);
        }

        #[mz_ore::test]
        #[cfg_attr(miri, ignore)] // slow
        fn cluster_startup_epoch_protobuf_roundtrip(expect in any::<ClusterStartupEpoch>() ) {
            let actual = protobuf_roundtrip::<_, ProtoClusterStartupEpoch>(&expect);
            assert_ok!(actual);
            assert_eq!(actual.unwrap(), expect);
        }
    }
}
