// Copyright (c) 2022-2023 Yuki Kishimoto
// Distributed under the MIT software license

//! Kind

use std::fmt;
use std::num::ParseIntError;
use std::str::FromStr;

use serde::de::{Deserialize, Deserializer, Error, Visitor};
use serde::ser::{Serialize, Serializer};

/// Event [`Kind`]
#[derive(Debug, Copy, Clone, Eq, Ord, PartialOrd)]
pub enum Kind {
    /// Initialize Group 
    InitializeGroup,
    /// Update Group 
    UpdateGroup,
    /// Initialize Repository 
    InitializeRepo,
    /// Update Repository 
    UpdateRepo,
    /// Initialize Branch 
    InitializeBranch,
    /// Update Branch
    UpdateBranch,
    /// Patch
    Patch,
    /// Pull Request
    PullRequest,
    /// Merge
    Merge,
    /// Custom
    Custom(u64),
}

impl Kind {
    /// Get [`Kind`] as `u32`
    pub fn as_u32(&self) -> u32 {
        self.as_u64() as u32
    }

    /// Get [`Kind`] as `u64`
    pub fn as_u64(&self) -> u64 {
        (*self).into()
    }

    /// Convert to nostr::event::Kind::Custom()
    pub fn into_sdk_custom_kind(&self) -> nostr::event::Kind {
        nostr::event::Kind::Custom((*self).into())
    }
}

impl From<u64> for Kind {
    fn from(u: u64) -> Self {
        match u {
            40000 => Self::InitializeGroup,
            40001 => Self::UpdateGroup,
            40010 => Self::InitializeRepo,
            40011 => Self::UpdateRepo,
            40020 => Self::InitializeBranch,
            40021 => Self::UpdateBranch,
            410 => Self::Patch,
            1 => Self::PullRequest,
            421 => Self::Merge,
            x => Self::Custom(x),

        }
    }
}

impl From<Kind> for u64 {
    fn from(e: Kind) -> u64 {
        match e {
            Kind::InitializeGroup => 40000,
            Kind::UpdateGroup => 40001,
            Kind::InitializeRepo => 40010,
            Kind::UpdateRepo => 40011,
            Kind::InitializeBranch => 40020,
            Kind::UpdateBranch => 40021,
            Kind::Patch => 410,
            Kind::PullRequest => 1,
            Kind::Merge => 421,
            Kind::Custom(u) => u,
        }
    }
}

impl FromStr for Kind {
    type Err = ParseIntError;
    fn from_str(kind: &str) -> Result<Self, Self::Err> {
        let kind: u64 = kind.parse()?;
        Ok(Self::from(kind))
    }
}

impl PartialEq<Kind> for Kind {
    fn eq(&self, other: &Kind) -> bool {
        self.as_u64() == other.as_u64()
    }
}

impl Serialize for Kind {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u64(From::from(*self))
    }
}

impl<'de> Deserialize<'de> for Kind {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_u64(KindVisitor)
    }
}

struct KindVisitor;

impl Visitor<'_> for KindVisitor {
    type Value = Kind;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "an unsigned number")
    }

    fn visit_u64<E>(self, v: u64) -> Result<Kind, E>
    where
        E: Error,
    {
        Ok(From::<u64>::from(v))
    }
}
