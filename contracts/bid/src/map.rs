// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.
//
// Copyright (c) DUSK NETWORK. All rights reserved.

use canonical::CanonError;
use canonical_derive::Canon;
use core::ops::Deref;
use dusk_bytes::Serializable;
use dusk_hamt::Map;
use dusk_pki::PublicKey;

#[derive(Default, Debug, Clone, Canon)]
pub struct KeyToIdxMap(Map<[u8; 32], u64>);

impl KeyToIdxMap {
    /// Create a new instance of a [`KeyToIdxMap`].
    pub fn new() -> KeyToIdxMap {
        Self(Hamt::<[u8; 32], u64, ()>::default())
    }

    /// Include a key -> value mapping to the set.
    ///
    /// If the key was previously mapped, it will return the old value in the
    /// form `Ok(Some(u64))`.
    ///
    /// If the key was not previously mappen, the return will be `Ok(None)`
    pub fn insert(
        &mut self,
        pk: PublicKey,
        bid_idx: usize,
    ) -> Result<Option<u64>, CanonError> {
        self.0.insert(pk.to_bytes(), bid_idx as u64)
    }

    /// Fetch a previously inserted key -> value mapping, provided the key.
    ///
    /// Will returnNone)` if no correspondent key was found.
    pub fn get(
        &self,
        pk: PublicKey,
    ) -> Result<Option<impl Deref<Target = u64> + '_>, CanonError> {
        self.0.get(&pk.to_bytes())
    }

    /// Remove an entry from the tree. It will return `Ok(Some(u64))` in case
    /// the key exists and `Ok(None)` otherways.
    pub fn remove(&mut self, pk: PublicKey) -> Result<Option<u64>, CanonError> {
        self.0.remove(&pk.to_bytes())
    }
}
