/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::fmt::Write;

use allocative::Allocative;
use buck2_core::base_deferred_key::BaseDeferredKey;
use buck2_core::fs::dynamic_actions_action_key::DynamicActionsActionKey;
use dupe::Dupe;

use crate::deferred::key::DeferredHolderKey;

/// The base key. We can actually get rid of this and just use 'DeferredKey' if rule analysis is an
/// 'Deferred' itself. This is used to construct the composed 'DeferredKey::Deferred' or
/// 'DeferredKey::Base' type.
#[derive(
    Hash,
    Eq,
    PartialEq,
    Clone,
    Dupe,
    derive_more::Display,
    Debug,
    Allocative
)]
#[display("{_0}_{_1}")]
pub struct DynamicLambdaResultsKey(DeferredHolderKey, DynamicLambdaIndex);

impl DynamicLambdaResultsKey {
    pub fn new(key: DeferredHolderKey, idx: DynamicLambdaIndex) -> Self {
        Self(key, idx)
    }

    pub fn owner(&self) -> &BaseDeferredKey {
        self.0.owner()
    }

    pub fn holder_key(&self) -> &DeferredHolderKey {
        &self.0
    }

    pub fn action_key(&self) -> DynamicActionsActionKey {
        let mut v = self.0.action_key();
        write!(&mut v, "_{}", self.1).unwrap();
        DynamicActionsActionKey::new(&v)
    }
}

#[derive(
    Debug,
    Eq,
    PartialEq,
    Hash,
    Clone,
    Dupe,
    Copy,
    derive_more::Display,
    Allocative
)]
pub struct DynamicLambdaIndex(u32);

impl DynamicLambdaIndex {
    pub fn new(v: u32) -> Self {
        Self(v)
    }
}
