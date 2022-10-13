use anyhow::{anyhow, Error};
use cid::multihash::{Code, MultihashDigest};
use cid::Cid;
use fvm_ipld_blockstore::Blockstore;
use fvm_ipld_encoding::{to_vec, Cbor, CborStore, RawBytes, DAG_CBOR};
use fvm_sdk as fvm;
use fvm_sdk::NO_DATA_BLOCK_ID;
use fvm_shared::actor::builtin::Type;
use fvm_shared::address::Address;
use fvm_shared::clock::ChainEpoch;
use fvm_shared::crypto::signature::Signature;
use fvm_shared::econ::TokenAmount;
use fvm_shared::error::{ErrorNumber, ExitCode};
use fvm_shared::piece::PieceInfo;
use fvm_shared::randomness::Randomness;
use fvm_shared::sector::{
    AggregateSealVerifyProofAndInfos, RegisteredSealProof, ReplicaUpdateInfo, SealVerifyInfo,
    WindowPoStVerifyInfo,
};
use fvm_shared::version::NetworkVersion;
use fvm_shared::{ActorID, MethodNum};
#[cfg(feature = "fake-proofs")]
use sha2::{Digest, Sha256};

use crate::runtime::actor_blockstore::ActorBlockstore;
use crate::runtime::{
    ActorCode, ConsensusFault, DomainSeparationTag, MessageInfo, Policy, Primitives,
    Verifier,
};
use crate::{actor_error, ActorError, Runtime};

lazy_static! {
    /// Cid of the empty array Cbor bytes (`EMPTY_ARR_BYTES`).
    pub static ref EMPTY_ARR_CID: Cid = {
        let empty = to_vec::<[(); 0]>(&[]).unwrap();
        Cid::new_v1(DAG_CBOR, Code::Blake2b256.digest(&empty))
    };
}

/// A runtime that bridges to the FVM environment through the FVM SDK.
pub struct FvmRuntime<B = ActorBlockstore> {
    blockstore: B,
    /// Indicates whether we are in a state transaction. During such, sending
    /// messages is prohibited.
    in_transaction: bool,
    /// Indicates that the caller has been validated.
    caller_validated: bool,
    /// The runtime policy
    policy: Policy,
}

impl Default for FvmRuntime {
    fn default() -> Self {
        FvmRuntime {
            blockstore: ActorBlockstore,
            in_transaction: false,
            caller_validated: false,
            policy: Policy::default(),
        }
    }
}

impl<B> FvmRuntime<B> {
    fn assert_not_validated(&mut self) -> Result<(), ActorError> {
        if self.caller_validated {
            return Err(actor_error!(
                assertion_failed,
                "Method must validate caller identity exactly once"
            ));
        }
        Ok(())
    }

    #[allow(dead_code)]
    fn policy_mut(&mut self) -> &mut Policy {
        &mut self.policy
    }
}

/// A stub MessageInfo implementation performing FVM syscalls to obtain its fields.
struct FvmMessage;

impl MessageInfo for FvmMessage {
    fn caller(&self) -> Address {
        Address::new_id(fvm::message::caller())
    }

    fn receiver(&self) -> Address {
        Address::new_id(fvm::message::receiver())
    }

    fn value_received(&self) -> TokenAmount {
        fvm::message::value_received()
    }
}

impl<B> Runtime<B> for FvmRuntime<B>
where
    B: Blockstore,
{
    fn network_version(&self) -> NetworkVersion {
        fvm::network::version()
    }

    fn message(&self) -> &dyn MessageInfo {
        &FvmMessage
    }

    fn curr_epoch(&self) -> ChainEpoch {
        fvm::network::curr_epoch()
    }

    fn validate_immediate_caller_accept_any(&mut self) -> Result<(), ActorError> {
        self.assert_not_validated()?;
        self.caller_validated = true;
        Ok(())
    }

    fn validate_immediate_caller_is<'a, I>(&mut self, addresses: I) -> Result<(), ActorError>
    where
        I: IntoIterator<Item = &'a Address>,
    {
        self.assert_not_validated()?;
        let caller_addr = self.message().caller();
        if addresses.into_iter().any(|a| *a == caller_addr) {
            self.caller_validated = true;
            Ok(())
        } else {
            Err(actor_error!(forbidden;
                "caller {} is not one of supported", caller_addr
            ))
        }
    }

    fn validate_immediate_caller_type<'a, I>(&mut self, types: I) -> Result<(), ActorError>
    where
        I: IntoIterator<Item = &'a Type>,
    {
        self.assert_not_validated()?;
        let caller_cid = {
            let caller_addr = self.message().caller();
            self.get_actor_code_cid(&caller_addr).expect("failed to lookup caller code")
        };

        match self.resolve_builtin_actor_type(&caller_cid) {
            Some(typ) if types.into_iter().any(|t| *t == typ) => {
                self.caller_validated = true;
                Ok(())
            }
            _ => Err(actor_error!(forbidden;
                    "caller cid type {} not one of supported", caller_cid)),
        }
    }

    fn current_balance(&self) -> TokenAmount {
        fvm::sself::current_balance()
    }

    fn resolve_address(&self, address: &Address) -> Option<Address> {
        fvm::actor::resolve_address(address).map(Address::new_id)
    }

    fn get_actor_code_cid(&self, addr: &Address) -> Option<Cid> {
        fvm::actor::get_actor_code_cid(addr)
    }

    fn get_randomness_from_tickets(
        &self,
        personalization: DomainSeparationTag,
        rand_epoch: ChainEpoch,
        entropy: &[u8],
    ) -> Result<Randomness, ActorError> {
        // Note: For Go actors, Lotus treated all failures to get randomness as "fatal" errors,
        // which it then translated into exit code SysErrReserved1 (= 4, and now known as
        // SYS_ILLEGAL_INSTRUCTION), rather than just aborting with an appropriate exit code.
        //
        // We can replicate that here prior to network v16, but from nv16 onwards the FVM will
        // override the attempt to use a system exit code, and produce
        // SYS_ILLEGAL_EXIT_CODE (9) instead.
        //
        // Since that behaviour changes, we may as well abort with a more appropriate exit code
        // explicitly.
        fvm::rand::get_chain_randomness(personalization as i64, rand_epoch, entropy).map_err(|e| {
            if self.network_version() < NetworkVersion::V16 {
                ActorError::unchecked(ExitCode::SYS_ILLEGAL_INSTRUCTION,
                    "failed to get chain randomness".into())
            } else {
                match e {
                    ErrorNumber::LimitExceeded => {
                        actor_error!(illegal_argument; "randomness lookback exceeded: {}", e)
                    }
                    e => actor_error!(assertion_failed; "get chain randomness failed with an unexpected error: {}", e),
                }
            }
        })
    }

    fn get_randomness_from_beacon(
        &self,
        personalization: DomainSeparationTag,
        rand_epoch: ChainEpoch,
        entropy: &[u8],
    ) -> Result<Randomness, ActorError> {
        // See note on exit codes in get_randomness_from_tickets.
        fvm::rand::get_beacon_randomness(personalization as i64, rand_epoch, entropy).map_err(|e| {
            if self.network_version() < NetworkVersion::V16 {
                ActorError::unchecked(ExitCode::SYS_ILLEGAL_INSTRUCTION,
                    "failed to get chain randomness".into())
            } else {
                match e {
                    ErrorNumber::LimitExceeded => {
                        actor_error!(illegal_argument; "randomness lookback exceeded: {}", e)
                    }
                    e => actor_error!(assertion_failed; "get chain randomness failed with an unexpected error: {}", e),
                }
            }
        })
    }

    fn create<C: Cbor>(&mut self, obj: &C) -> Result<(), ActorError> {
        let root = fvm::sself::root()?;
        if root != *EMPTY_ARR_CID {
            return Err(
                actor_error!(illegal_state; "failed to create state; expected empty array CID, got: {}", root),
            );
        }
        let new_root = ActorBlockstore.put_cbor(obj, Code::Blake2b256)
            .map_err(|e| actor_error!(illegal_argument; "failed to write actor state during creation: {}", e.to_string()))?;
        fvm::sself::set_root(&new_root)?;
        Ok(())
    }

    fn state<C: Cbor>(&self) -> Result<C, ActorError> {
        let root = fvm::sself::root()?;
        Ok(ActorBlockstore
            .get_cbor(&root)
            .map_err(|_| actor_error!(illegal_argument; "failed to get actor for Readonly state"))?
            .expect("State does not exist for actor state root"))
    }

    fn transaction<C, RT, F>(&mut self, f: F) -> Result<RT, ActorError>
    where
        C: Cbor,
        F: FnOnce(&mut C, &mut Self) -> Result<RT, ActorError>,
    {
        let state_cid = fvm::sself::root()
            .map_err(|_| actor_error!(illegal_argument; "failed to get actor root state CID"))?;

        log::debug!("getting cid: {}", state_cid);

        let mut state = ActorBlockstore
            .get_cbor::<C>(&state_cid)
            .map_err(|_| actor_error!(illegal_argument; "failed to get actor state"))?
            .expect("State does not exist for actor state root");

        self.in_transaction = true;
        let result = f(&mut state, self);
        self.in_transaction = false;

        let ret = result?;
        let new_root = ActorBlockstore.put_cbor(&state, Code::Blake2b256)
            .map_err(|e| actor_error!(illegal_argument; "failed to write actor state in transaction: {}", e.to_string()))?;
        fvm::sself::set_root(&new_root)?;
        Ok(ret)
    }

    fn store(&self) -> &B {
        &self.blockstore
    }

    fn send(
        &self,
        to: Address,
        method: MethodNum,
        params: RawBytes,
        value: TokenAmount,
    ) -> Result<RawBytes, ActorError> {
        if self.in_transaction {
            return Err(actor_error!(assertion_failed; "send is not allowed during transaction"));
        }
        match fvm::send::send(&to, method, params, value) {
            Ok(ret) => {
                if ret.exit_code.is_success() {
                    Ok(ret.return_data)
                } else {
                    // The returned code can't be simply propagated as it may be a system exit code.
                    // TODO: improve propagation once we return a RuntimeError.
                    // Ref https://github.com/filecoin-project/builtin-actors/issues/144
                    let exit_code = match ret.exit_code {
                        // This means the called actor did something wrong. We can't "make up" a
                        // reasonable exit code.
                        ExitCode::SYS_MISSING_RETURN
                        | ExitCode::SYS_ILLEGAL_INSTRUCTION
                        | ExitCode::SYS_ILLEGAL_EXIT_CODE => ExitCode::USR_UNSPECIFIED,
                        // We don't expect any other system errors.
                        code if code.is_system_error() => ExitCode::USR_ASSERTION_FAILED,
                        // Otherwise, pass it through.
                        code => code,
                    };
                    Err(ActorError::unchecked(
                        exit_code,
                        format!(
                            "send to {} method {} aborted with code {}",
                            to, method, ret.exit_code
                        ),
                    ))
                }
            }
            Err(err) => Err(match err {
                // Some of these errors are from operations in the Runtime or SDK layer
                // before or after the underlying VM send syscall.
                ErrorNumber::NotFound => {
                    // This means that the receiving actor doesn't exist.
                    // TODO: we can't reasonably determine the correct "exit code" here.
                    actor_error!(unspecified; "receiver not found")
                }
                ErrorNumber::InsufficientFunds => {
                    // This means that the send failed because we have insufficient funds. We will
                    // get a _syscall error_, not an exit code, because the target actor will not
                    // run (and therefore will not exit).
                    actor_error!(insufficient_funds; "not enough funds")
                }
                ErrorNumber::LimitExceeded => {
                    // This means we've exceeded the recursion limit.
                    // TODO: Define a better exit code.
                    actor_error!(assertion_failed; "recursion limit exceeded")
                }
                err => {
                    // We don't expect any other syscall exit codes.
                    actor_error!(assertion_failed; "unexpected error: {}", err)
                }
            }),
        }
    }

    fn new_actor_address(&mut self) -> Result<Address, ActorError> {
        Ok(fvm::actor::new_actor_address())
    }

    fn create_actor(&mut self, code_id: Cid, actor_id: ActorID) -> Result<(), ActorError> {
        if self.in_transaction {
            return Err(
                actor_error!(assertion_failed; "create_actor is not allowed during transaction"),
            );
        }
        fvm::actor::create_actor(actor_id, &code_id).map_err(|e| match e {
            ErrorNumber::IllegalArgument => {
                ActorError::illegal_argument("failed to create actor".into())
            }
            _ => actor_error!(assertion_failed; "create failed with unknown error: {}", e),
        })
    }

    fn delete_actor(&mut self, beneficiary: &Address) -> Result<(), ActorError> {
        if self.in_transaction {
            return Err(
                actor_error!(assertion_failed; "delete_actor is not allowed during transaction"),
            );
        }
        Ok(fvm::sself::self_destruct(beneficiary)?)
    }

    fn resolve_builtin_actor_type(&self, code_id: &Cid) -> Option<Type> {
        fvm::actor::get_builtin_actor_type(code_id)
    }

    fn get_code_cid_for_type(&self, typ: Type) -> Cid {
        fvm::actor::get_code_cid_for_type(typ)
    }

    fn total_fil_circ_supply(&self) -> TokenAmount {
        fvm::network::total_fil_circ_supply()
    }

    fn charge_gas(&mut self, name: &'static str, compute: i64) {
        fvm::gas::charge(name, compute as u64)
    }

    fn base_fee(&self) -> TokenAmount {
        fvm::network::base_fee()
    }
}

impl<B> Primitives for FvmRuntime<B>
where
    B: Blockstore,
{
    fn verify_signature(
        &self,
        signature: &Signature,
        signer: &Address,
        plaintext: &[u8],
    ) -> Result<(), Error> {
        match fvm::crypto::verify_signature(signature, signer, plaintext) {
            Ok(true) => Ok(()),
            Ok(false) | Err(_) => Err(Error::msg("invalid signature")),
        }
    }

    fn hash_blake2b(&self, data: &[u8]) -> [u8; 32] {
        fvm::crypto::hash_blake2b(data)
    }

    fn compute_unsealed_sector_cid(
        &self,
        proof_type: RegisteredSealProof,
        pieces: &[PieceInfo],
    ) -> Result<Cid, Error> {
        // The only actor that invokes this (market actor) is generating the
        // exit code ErrIllegalArgument. We should probably move that here, or to the syscall itself.
        fvm::crypto::compute_unsealed_sector_cid(proof_type, pieces)
            .map_err(|e| anyhow!("failed to compute unsealed sector CID; exit code: {}", e))
    }
}

/// A convenience function that built-in actors can delegate their execution to.
///
/// The trampoline takes care of boilerplate:
///
/// 0.  Initialize logging if debugging is enabled.
/// 1.  Obtains the parameter data from the FVM by fetching the parameters block.
/// 2.  Obtains the method number for the invocation.
/// 3.  Creates an FVM runtime shim.
/// 4.  Invokes the target method.
/// 5a. In case of error, aborts the execution with the emitted exit code, or
/// 5b. In case of success, stores the return data as a block and returns the latter.
pub fn trampoline<C: ActorCode>(params: u32) -> u32 {
    fvm::debug::init_logging();

    std::panic::set_hook(Box::new(|info| {
        fvm::vm::abort(ExitCode::USR_ASSERTION_FAILED.value(), Some(&format!("{}", info)))
    }));

    let method = fvm::message::method_number();
    log::debug!("fetching parameters block: {}", params);
    let params = fvm::message::params_raw(params).expect("params block invalid").1;
    let params = RawBytes::new(params);
    log::debug!("input params: {:x?}", params.bytes());

    // Construct a new runtime.
    let mut rt = FvmRuntime::default();
    // Invoke the method, aborting if the actor returns an errored exit code.
    let ret = C::invoke_method(&mut rt, method, &params)
        .unwrap_or_else(|err| fvm::vm::abort(err.exit_code().value(), Some(err.msg())));

    // Abort with "assertion failed" if the actor failed to validate the caller somewhere.
    // We do this after handling the error, because the actor may have encountered an error before
    // it even could validate the caller.
    if !rt.caller_validated {
        fvm::vm::abort(ExitCode::USR_ASSERTION_FAILED.value(), Some("failed to validate caller"))
    }

    // Then handle the return value.
    if ret.is_empty() {
        NO_DATA_BLOCK_ID
    } else {
        fvm::ipld::put_block(DAG_CBOR, ret.bytes()).expect("failed to write result")
    }
}
