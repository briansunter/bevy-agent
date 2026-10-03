use crate::*;

#[derive(
    Component, Clone, Copy, Debug, Serialize, Deserialize, schemars::JsonSchema, PartialEq, Eq, Hash,
)]
pub struct StableEntityId(pub u128);

impl StableEntityId {
    #[must_use]
    pub const fn from_u64(value: u64) -> Self {
        Self(value as u128)
    }
}

#[derive(
    Component, Clone, Copy, Debug, Serialize, Deserialize, schemars::JsonSchema, PartialEq, Eq, Hash,
)]
pub struct SnapshotEntity;

#[derive(Resource, Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct StableIdAllocator {
    pub next: u128,
}

impl Default for StableIdAllocator {
    fn default() -> Self {
        Self { next: 1 }
    }
}

impl StableIdAllocator {
    pub fn allocate(&mut self) -> StableEntityId {
        let id = StableEntityId(self.next);
        self.next += 1;
        id
    }
}

#[derive(
    Clone,
    Copy,
    Debug,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
)]
pub struct SnapshotId(pub Uuid);

impl SnapshotId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for SnapshotId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, schemars::JsonSchema, PartialEq, Eq, Hash)]
pub struct TimelineId(pub Uuid);

impl TimelineId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for TimelineId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(
    Clone,
    Copy,
    Debug,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
)]
pub struct BranchId(pub Uuid);

impl BranchId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for BranchId {
    fn default() -> Self {
        Self::new()
    }
}
