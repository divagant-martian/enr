//! Update operations over the [`Enr`].

use bytes::Bytes;

use crate::{Enr, EnrKey, EnrPublicKey, Key, NodeId, MAX_ENR_SIZE};

mod ops;

use ops::{Op, Update};

/// An update guard over the [`Enr`].
/// The inverses are set as a generic to allow optimizing for single updates, multiple updates with
/// a known count of updates and arbitrary updates.
pub struct Guard<'a, K: EnrKey, I> {
    /// [`Enr`] with update [`Op`]s already applied.
    enr: &'a mut Enr<K>,
    /// Inverses that would need to be applied to the [`Enr`] to restore [`Enr::content`].
    ///
    /// Inverses must be in the order in which they were obtained, so that applying them in
    /// reserved order produces the original content.
    inverses: I,
}

/// Implementation for a single update
impl<'a, K: EnrKey> Guard<'a, K, Op> {
    /// Create a new guard verifying the update and applying it to the the [`Enr`].
    /// If validation fails, it's guaranteed that the [`Enr`] has not been changed with
    /// an error returned.
    // NOTE: this is expanded to n-tuples via macros
    pub fn new(enr: &'a mut Enr<K>, update: Update) -> Result<Self, Error> {
        // validate the update
        let update = update.to_valid_op()?;
        // apply the valid operation to the enr and create the inverse
        let inverses = update.apply_and_invert(enr);
        Ok(Self { enr, inverses })
    }
}

/// Implementation for an arbitrary number of updates.
impl<'a, K: EnrKey> Guard<'a, K, Vec<Op>> {
    /// Create a new guard verifying the updates and applying them to the the [`Enr`].
    /// If validation fails, it's guaranteed that the [`Enr`] has not been changed with
    /// an error returned.
    pub fn new(enr: &'a mut Enr<K>, updates: Vec<Update>) -> Result<Self, Error> {
        // validate all operations before applying any
        let valid = updates
            .into_iter()
            .map(Update::to_valid_op)
            .collect::<Result<Vec<Op>, Error>>()?;
        // apply the valid operations to the enr and create a tuple with the inverses in
        // case they need to be reverted
        let inverses = valid
            .into_iter()
            .map(|update| update.apply_and_invert(enr))
            .collect();
        Ok(Self { enr, inverses })
    }
}

/* Implementation for tuples*/

/// Map an identifier inside a macro to a type
macro_rules! map_to_type {
    ($in: ident, $out: ident) => {
        $out
    };
}
/// Generates the implementation of PreUpdate::apply to a tuple of 2 or more. The macro arguments
/// are the number of variables needed to map Update intents to valid operations.
///
/// A valid call of this macro looks like
/// `gen_pre_update_impl!(up0, up1)`
///
/// This generates the implementation for `PreUpdate<'a, K, (Update, Update)>`
/// containing the function
/// `pub fn apply(self) -> Result<Guard<'a, K, (Op, Op)>, Error> { .. }`
macro_rules! gen_pre_update_impl {
    ($($up: ident,)*) => {
        impl<'a, K: EnrKey> Guard<'a, K, ($(map_to_type!($up, Op),)*)> {
            /// Verify all updates and apply them to the enr, returning a [`Guard`]. If validation
            /// of any update fails, it's guaranteed that the [`Enr`] has not been changed.
            pub fn new(enr: &'a mut Enr<K>, updates: ($(map_to_type!($up, Update),)*)) -> Result<Self, Error> {
                // destructure the tuple using the identifiers
                let ($($up,)*) = updates;
                // reasing to the identifiers the valid version of each update
                let ($($up,)*) = ($($up.to_valid_op()?,)*);
                // apply the valid operations to the enr and create a tuple with the inverses in
                // case they need to be reverted
                let inverses = ($($up.apply_and_invert(enr),)*);
                Ok(Self { enr, inverses })
            }
        }
    };
}

/// Calls `gen_pre_update_impl` for all tuples of size in the range [2; N], where N is the number
/// of identifies received.
macro_rules! gen_ntuple_pre_update_impls {
    ($up: ident, $($tokens: tt)+) => {
        gen_pre_update_impl!($up, $($tokens)*);
        gen_ntuple_pre_update_impls!($($tokens)*);
    };
    ($up: ident,) => {};
}

gen_ntuple_pre_update_impls!(up0, up1, up2,);

impl<'a, K: EnrKey, I> Guard<'a, K, I> {
    /// Applies the remaining operations in a valid [`Enr`] update:
    ///
    /// 1. Add the public key matching the signing key to the contents.
    /// 2. Update the sequence number.
    /// 3. Sign the [`Enr`].
    /// 4. Verify that the encoded [`Enr`] is within spec lengths.
    /// 5. Update the cache'd node id
    ///
    /// If any of these steps fails, a [`Revert`] object is returned that allows to reset the
    /// [`Enr`] and obtain the error that occurred.
    pub fn finish(self, signing_key: &K) -> Result<I, Revert<'a, K, I>> {
        let Guard { enr, inverses } = self;
        let mut revert = RevertOps::new(inverses);

        // 1. set the public key
        let public_key = signing_key.public();
        revert.key = enr.content.insert(
            public_key.enr_key(),
            rlp::encode(&public_key.encode().as_ref()).freeze(),
        );

        // 2. set the new sequence number
        revert.seq = Some(enr.seq());
        enr.seq = match enr.seq.checked_add(1) {
            Some(seq) => seq,
            None => {
                return Err(Revert {
                    enr,
                    pending: revert,
                    error: Error::SequenceNumberTooHigh,
                })
            }
        };

        // 3. sign the ENR
        revert.signature = Some(enr.signature.clone());
        enr.signature = match enr.compute_signature(signing_key) {
            Ok(signature) => signature,
            Err(_) => {
                return Err(Revert {
                    enr,
                    pending: revert,
                    error: Error::SigningError,
                })
            }
        };

        // the size of the node id is fixed, and its encoded size depends exclusively on the data
        // size, so we first check the size and then update the node id. This allows us to not need
        // to track the previous node id in case of failure since this is the last step

        // 4. check the encoded size
        if enr.size() > MAX_ENR_SIZE {
            return Err(Revert {
                enr,
                pending: revert,
                error: Error::ExceedsMaxSize,
            });
        }

        // 5. update the node_id
        enr.node_id = NodeId::from(public_key);

        // nothing to revert, return the content inverses since those identify what was done
        let RevertOps {
            content_inverses, ..
        } = revert;
        Ok(content_inverses)
    }
}


pub enum Error {
    /// The ENR is too large.
    ExceedsMaxSize,
    /// The sequence number is too large.
    SequenceNumberTooHigh,
    /// There was an error with signing an ENR record.
    SigningError,
    /// The identity scheme is not supported.
    UnsupportedIdentityScheme,
    /// Data is valid RLP but the contents do not represent the expected type for the key.
    InvalidReservedKeyData(Key),
    /// The entered RLP data is invalid.
    InvalidRlpData(rlp::DecoderError),
}

///
pub struct Revert<'a, K: EnrKey, I> {
    enr: &'a mut Enr<K>,
    pending: RevertOps<I>,
    error: Error,
}

pub struct RevertOps<I> {
    content_inverses: I,
    key: Option<Bytes>,
    seq: Option<u64>,
    signature: Option<Vec<u8>>,
}

impl<I> RevertOps<I> {
    fn new(content_inverses: I) -> Self {
        RevertOps {
            content_inverses,
            key: None,
            seq: None,
            signature: None,
        }
    }
}
