// Copyright 2019-2022 ChainSafe Systems
// SPDX-License-Identifier: Apache-2.0, MIT

use core::fmt;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::rc::Rc;

use cid::multihash::{Code, Multihash as OtherMultihash};
use cid::Cid;
use fvm_ipld_blockstore::{Blockstore, MemoryBlockstore};
use fvm_ipld_encoding::de::DeserializeOwned;
use fvm_ipld_encoding::ipld_block::IpldBlock;
use fvm_ipld_encoding::CborStore;
use fvm_shared::address::{Address, Protocol};
use fvm_shared::clock::ChainEpoch;
use serde::Serialize;

use fvm_shared::commcid::{FIL_COMMITMENT_SEALED, FIL_COMMITMENT_UNSEALED};
use fvm_shared::crypto::signature::Signature;
use fvm_shared::econ::TokenAmount;
use fvm_shared::error::ExitCode;
use fvm_shared::version::NetworkVersion;
use fvm_shared::{ActorID, MethodNum};

use multihash::derive::Multihash;
use multihash::MultihashDigest;

use rand::prelude::*;

use crate::runtime::{ActorCode, MessageInfo, Primitives, Runtime};
use crate::{actor_error, ActorError, Type};

type Func = dyn Fn(&[u8]) -> [u8; 32];

lazy_static! {
    pub static ref SYSTEM_ACTOR_CODE_ID: Cid = make_builtin(b"fil/test/system");
    pub static ref INIT_ACTOR_CODE_ID: Cid = make_builtin(b"fil/test/init");
    pub static ref CRON_ACTOR_CODE_ID: Cid = make_builtin(b"fil/test/cron");
    pub static ref ACCOUNT_ACTOR_CODE_ID: Cid = make_builtin(b"fil/test/account");
    pub static ref POWER_ACTOR_CODE_ID: Cid = make_builtin(b"fil/test/storagepower");
    pub static ref MINER_ACTOR_CODE_ID: Cid = make_builtin(b"fil/test/storageminer");
    pub static ref MARKET_ACTOR_CODE_ID: Cid = make_builtin(b"fil/test/storagemarket");
    pub static ref PAYCH_ACTOR_CODE_ID: Cid = make_builtin(b"fil/test/paymentchannel");
    pub static ref MULTISIG_ACTOR_CODE_ID: Cid = make_builtin(b"fil/test/multisig");
    pub static ref REWARD_ACTOR_CODE_ID: Cid = make_builtin(b"fil/test/reward");
    pub static ref VERIFREG_ACTOR_CODE_ID: Cid = make_builtin(b"fil/test/verifiedregistry");
    pub static ref SCA_ACTOR_CODE_ID: Cid = make_builtin(b"fil/test/sca");
    pub static ref SUBNET_ACTOR_CODE_ID: Cid = make_builtin(b"fil/test/subnet");
    pub static ref ACTOR_TYPES: BTreeMap<Cid, Type> = {
        let mut map = BTreeMap::new();
        map.insert(*SYSTEM_ACTOR_CODE_ID, Type::System);
        map.insert(*INIT_ACTOR_CODE_ID, Type::Init);
        map.insert(*CRON_ACTOR_CODE_ID, Type::Cron);
        map.insert(*ACCOUNT_ACTOR_CODE_ID, Type::Account);
        map.insert(*POWER_ACTOR_CODE_ID, Type::Power);
        map.insert(*MINER_ACTOR_CODE_ID, Type::Miner);
        map.insert(*MARKET_ACTOR_CODE_ID, Type::Market);
        map.insert(*PAYCH_ACTOR_CODE_ID, Type::PaymentChannel);
        map.insert(*MULTISIG_ACTOR_CODE_ID, Type::Multisig);
        map.insert(*REWARD_ACTOR_CODE_ID, Type::Reward);
        map.insert(*VERIFREG_ACTOR_CODE_ID, Type::VerifiedRegistry);
        map
    };
    pub static ref CALLER_TYPES_SIGNABLE: Vec<Cid> =
        vec![*ACCOUNT_ACTOR_CODE_ID, *MULTISIG_ACTOR_CODE_ID];
    pub static ref NON_SINGLETON_CODES: BTreeMap<Cid, ()> = {
        let mut map = BTreeMap::new();
        map.insert(*ACCOUNT_ACTOR_CODE_ID, ());
        map.insert(*PAYCH_ACTOR_CODE_ID, ());
        map.insert(*MULTISIG_ACTOR_CODE_ID, ());
        map.insert(*MINER_ACTOR_CODE_ID, ());
        map
    };
}

const IPLD_RAW: u64 = 0x55;

/// Returns an identity CID for bz.
pub fn make_builtin(bz: &[u8]) -> Cid {
    Cid::new_v1(
        IPLD_RAW,
        OtherMultihash::wrap(0, bz).expect("name too long"),
    )
}

pub struct MockRuntime<BS = MemoryBlockstore> {
    pub epoch: ChainEpoch,
    pub miner: Address,
    pub base_fee: TokenAmount,
    pub id_addresses: HashMap<Address, Address>,
    pub actor_code_cids: HashMap<Address, Cid>,
    pub new_actor_addr: Option<Address>,
    pub receiver: Address,
    pub caller: Address,
    pub caller_type: Cid,
    pub value_received: TokenAmount,
    pub hash_func: Box<Func>,
    pub network_version: NetworkVersion,

    // Actor State
    pub state: Option<Cid>,
    pub balance: RefCell<TokenAmount>,

    // VM Impl
    pub in_call: bool,
    pub store: Rc<BS>,
    pub in_transaction: bool,

    // Expectations
    pub expectations: RefCell<Expectations>,

    pub circulating_supply: TokenAmount,
}

impl<BS> MockRuntime<BS> {
    pub fn new(store: BS) -> Self {
        Self {
            epoch: Default::default(),
            miner: Address::new_id(0),
            base_fee: Default::default(),
            id_addresses: Default::default(),
            actor_code_cids: Default::default(),
            new_actor_addr: Default::default(),
            receiver: Address::new_id(0),
            caller: Address::new_id(0),
            caller_type: Default::default(),
            value_received: Default::default(),
            hash_func: Box::new(blake2b_256),
            network_version: NetworkVersion::V0,
            state: Default::default(),
            balance: Default::default(),
            in_call: Default::default(),
            store: Rc::new(store),
            in_transaction: Default::default(),
            expectations: Default::default(),
            circulating_supply: Default::default(),
        }
    }
}

#[derive(Default)]
pub struct Expectations {
    pub expect_validate_caller_any: bool,
    pub expect_validate_caller_addr: Option<Vec<Address>>,
    pub expect_validate_caller_type: Option<Vec<Cid>>,
    pub expect_validate_caller_not_type: Option<Vec<Cid>>,
    pub expect_sends: VecDeque<ExpectedMessage>,
    pub expect_create_actor: Option<ExpectCreateActor>,
    pub expect_delete_actor: Option<bool>,
    pub expect_verify_sigs: VecDeque<ExpectedVerifySig>,
    pub expect_gas_charge: VecDeque<i64>,
}

impl Expectations {
    fn reset(&mut self) {
        *self = Default::default();
    }

    fn verify(&mut self) {
        assert!(
            !self.expect_validate_caller_any,
            "expected ValidateCallerAny, not received"
        );
        assert!(
            self.expect_validate_caller_addr.is_none(),
            "expected ValidateCallerAddr {:?}, not received",
            self.expect_validate_caller_addr
        );
        assert!(
            self.expect_validate_caller_type.is_none(),
            "expected ValidateCallerType {:?}, not received",
            self.expect_validate_caller_type
        );
        assert!(
            self.expect_sends.is_empty(),
            "expected all message to be send, unsent messages {:?}",
            self.expect_sends
        );
        assert!(
            self.expect_create_actor.is_none(),
            "expected actor to be created, uncreated actor: {:?}",
            self.expect_create_actor
        );
        assert!(
            self.expect_delete_actor.is_none(),
            "expected actor to be deleted: {:?}",
            self.expect_delete_actor
        );
        assert!(
            self.expect_verify_sigs.is_empty(),
            "expect_verify_sigs: {:?}, not received",
            self.expect_verify_sigs
        );
        assert!(
            self.expect_gas_charge.is_empty(),
            "expect_gas_charge {:?}, not received",
            self.expect_gas_charge
        );
    }
}

impl Default for MockRuntime {
    fn default() -> Self {
        Self {
            epoch: Default::default(),
            miner: Address::new_id(0),
            base_fee: Default::default(),
            id_addresses: Default::default(),
            actor_code_cids: Default::default(),
            new_actor_addr: Default::default(),
            receiver: Address::new_id(0),
            caller: Address::new_id(0),
            caller_type: Default::default(),
            value_received: Default::default(),
            hash_func: Box::new(blake2b_256),
            network_version: NetworkVersion::V0,
            state: Default::default(),
            balance: Default::default(),
            in_call: Default::default(),
            store: Default::default(),
            in_transaction: Default::default(),
            expectations: Default::default(),
            circulating_supply: Default::default(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ExpectCreateActor {
    pub code_id: Cid,
    pub actor_id: ActorID,
}

#[derive(Clone, Debug)]
pub struct ExpectedMessage {
    pub to: Address,
    pub method: MethodNum,
    pub params: Option<IpldBlock>,
    pub value: TokenAmount,

    // returns from applying expectedMessage
    pub send_return: Option<IpldBlock>,
    pub exit_code: ExitCode,
}

#[derive(Debug)]
pub struct ExpectedVerifySig {
    pub sig: Signature,
    pub signer: Address,
    pub plaintext: Vec<u8>,
    pub result: Result<(), anyhow::Error>,
}

#[derive(Clone, Debug)]
pub struct ExpectRandomness {}

pub fn expect_empty(res: Option<IpldBlock>) {
    assert!(res.is_none());
}

pub fn expect_abort_contains_message<T: fmt::Debug>(
    expect_exit_code: ExitCode,
    expect_msg: &str,
    res: Result<T, ActorError>,
) {
    let err = res.expect_err(&format!(
        "expected abort with exit code {expect_exit_code}, but call succeeded"
    ));
    assert_eq!(
        err.exit_code(),
        expect_exit_code,
        "expected failure with exit code {}, but failed with exit code {}; error message: {}",
        expect_exit_code,
        err.exit_code(),
        err.msg(),
    );
    let err_msg = err.msg();
    assert!(
        err.msg().contains(expect_msg),
        "expected err message '{err_msg}' to contain '{expect_msg}'",
    );
}

pub fn expect_abort<T: fmt::Debug>(exit_code: ExitCode, res: Result<T, ActorError>) {
    expect_abort_contains_message(exit_code, "", res);
}

impl<BS: Blockstore> MockRuntime<BS> {
    ///// Runtime access for tests /////

    pub fn get_state<T: DeserializeOwned>(&self) -> T {
        self.store_get(self.state.as_ref().unwrap())
    }

    pub fn replace_state<T: Serialize>(&mut self, obj: &T) {
        self.state = Some(self.store_put(obj));
    }

    pub fn set_balance(&mut self, amount: TokenAmount) {
        *self.balance.get_mut() = amount;
    }

    pub fn get_balance(&self) -> TokenAmount {
        self.balance.borrow().to_owned()
    }

    pub fn add_balance(&mut self, amount: TokenAmount) {
        *self.balance.get_mut() += amount;
    }

    pub fn set_value(&mut self, value: TokenAmount) {
        self.value_received = value;
    }

    pub fn set_caller(&mut self, code_id: Cid, address: Address) {
        self.caller = address;
        self.caller_type = code_id;
        self.actor_code_cids.insert(address, code_id);
    }

    pub fn set_address_actor_type(&mut self, address: Address, actor_type: Cid) {
        self.actor_code_cids.insert(address, actor_type);
    }

    pub fn get_id_address(&self, address: &Address) -> Option<Address> {
        if address.protocol() == Protocol::ID {
            return Some(*address);
        }
        self.id_addresses.get(address).cloned()
    }

    pub fn add_id_address(&mut self, source: Address, target: Address) {
        assert_eq!(
            target.protocol(),
            Protocol::ID,
            "target must use ID address protocol"
        );
        self.id_addresses.insert(source, target);
    }

    pub fn call<A: ActorCode>(
        &mut self,
        method_num: MethodNum,
        params: Option<IpldBlock>,
    ) -> Result<Option<IpldBlock>, ActorError> {
        self.in_call = true;
        let prev_state = self.state;
        let res = A::invoke_method(self, method_num, params);

        if res.is_err() {
            self.state = prev_state;
        }
        self.in_call = false;
        res
    }

    /// Method to use when we need to call something in the test that requires interacting
    /// with the runtime in a read-only fashion, but it's not an actor invocation.
    pub fn call_fn<F, T>(&mut self, f: F) -> anyhow::Result<T>
    where
        F: FnOnce(&mut Self) -> anyhow::Result<T>,
    {
        self.in_call = true;
        let res = f(self);
        self.in_call = false;
        res
    }

    /// Verifies that all mock expectations have been met.
    pub fn verify(&mut self) {
        self.expectations.borrow_mut().verify()
    }

    /// Clears all mock expectations.
    pub fn reset(&mut self) {
        self.expectations.borrow_mut().reset();
    }

    ///// Mock expectations /////

    #[allow(dead_code)]
    pub fn expect_validate_caller_addr(&mut self, addr: Vec<Address>) {
        assert!(!addr.is_empty(), "addrs must be non-empty");
        self.expectations.get_mut().expect_validate_caller_addr = Some(addr);
    }

    #[allow(dead_code)]
    pub fn expect_verify_signature(&self, exp: ExpectedVerifySig) {
        self.expectations
            .borrow_mut()
            .expect_verify_sigs
            .push_back(exp);
    }

    #[allow(dead_code)]
    pub fn expect_validate_caller_type(&mut self, types: Vec<Cid>) {
        assert!(!types.is_empty(), "addrs must be non-empty");
        self.expectations.borrow_mut().expect_validate_caller_type = Some(types);
    }

    #[allow(dead_code)]
    pub fn expect_validate_caller_not_type(&mut self, types: Vec<Cid>) {
        // we add type as an expectation to ensure that we did the type check
        // and then perform the explicit "not_type" check in the validate of
        // the MockRuntime
        self.expectations
            .borrow_mut()
            .expect_validate_caller_not_type = Some(types);
    }

    #[allow(dead_code)]
    pub fn expect_validate_caller_any(&self) {
        self.expectations.borrow_mut().expect_validate_caller_any = true;
    }

    #[allow(dead_code)]
    pub fn expect_delete_actor(&mut self, burn_unspent: bool) {
        self.expectations.borrow_mut().expect_delete_actor = Some(burn_unspent);
    }

    #[allow(dead_code)]
    pub fn expect_send(
        &mut self,
        to: Address,
        method: MethodNum,
        params: Option<IpldBlock>,
        value: TokenAmount,
        send_return: Option<IpldBlock>,
        exit_code: ExitCode,
    ) {
        self.expectations
            .borrow_mut()
            .expect_sends
            .push_back(ExpectedMessage {
                to,
                method,
                params,
                value,
                send_return,
                exit_code,
            })
    }

    #[allow(dead_code)]
    pub fn expect_create_actor(&mut self, code_id: Cid, actor_id: ActorID) {
        let a = ExpectCreateActor { code_id, actor_id };
        self.expectations.borrow_mut().expect_create_actor = Some(a);
    }

    #[allow(dead_code)]
    pub fn set_received(&mut self, amount: TokenAmount) {
        self.value_received = amount;
    }

    #[allow(dead_code)]
    pub fn set_base_fee(&mut self, base_fee: TokenAmount) {
        self.base_fee = base_fee;
    }

    #[allow(dead_code)]
    pub fn set_circulating_supply(&mut self, circ_supply: TokenAmount) {
        self.circulating_supply = circ_supply;
    }

    #[allow(dead_code)]
    pub fn set_epoch(&mut self, epoch: ChainEpoch) {
        self.epoch = epoch;
    }

    #[allow(dead_code)]
    pub fn expect_gas_charge(&mut self, value: i64) {
        self.expectations
            .borrow_mut()
            .expect_gas_charge
            .push_back(value);
    }

    ///// Private helpers /////

    fn require_in_call(&self) {
        assert!(
            self.in_call,
            "invalid runtime invocation outside of method call"
        )
    }

    fn store_put<T: Serialize>(&self, o: &T) -> Cid {
        self.store.put_cbor(&o, Code::Blake2b256).unwrap()
    }

    fn store_get<T: DeserializeOwned>(&self, cid: &Cid) -> T {
        self.store.get_cbor(cid).unwrap().unwrap()
    }
}

impl<BS> MessageInfo for MockRuntime<BS> {
    fn caller(&self) -> Address {
        self.caller
    }
    fn receiver(&self) -> Address {
        self.receiver
    }
    fn value_received(&self) -> TokenAmount {
        self.value_received.clone()
    }
}

impl<BS: Blockstore> Runtime for MockRuntime<BS> {
    type Blockstore = Rc<BS>;

    fn network_version(&self) -> NetworkVersion {
        self.network_version
    }

    fn message(&self) -> &dyn MessageInfo {
        self.require_in_call();
        self
    }

    fn curr_epoch(&self) -> ChainEpoch {
        self.require_in_call();
        self.epoch
    }

    fn validate_immediate_caller_accept_any(&mut self) -> Result<(), ActorError> {
        self.require_in_call();
        assert!(
            self.expectations.borrow_mut().expect_validate_caller_any,
            "unexpected validate-caller-any"
        );
        self.expectations.borrow_mut().expect_validate_caller_any = false;
        Ok(())
    }

    fn validate_immediate_caller_is<'a, I>(&mut self, addresses: I) -> Result<(), ActorError>
    where
        I: IntoIterator<Item = &'a Address>,
    {
        self.require_in_call();

        let addrs: Vec<Address> = addresses.into_iter().cloned().collect();

        let mut expectations = self.expectations.borrow_mut();
        assert!(
            expectations.expect_validate_caller_addr.is_some(),
            "unexpected validate caller addrs"
        );

        let expected_addrs = expectations.expect_validate_caller_addr.as_ref().unwrap();
        assert_eq!(
            &addrs, expected_addrs,
            "unexpected validate caller addrs {:?}, expected {:?}",
            addrs, &expectations.expect_validate_caller_addr
        );

        for expected in &addrs {
            if self.message().caller() == *expected {
                expectations.expect_validate_caller_addr = None;
                return Ok(());
            }
        }
        expectations.expect_validate_caller_addr = None;
        Err(actor_error!(forbidden;
                "caller address {:?} forbidden, allowed: {:?}",
                self.message().caller(), &addrs
        ))
    }

    fn validate_immediate_caller_type<'a, I>(&mut self, types: I) -> Result<(), ActorError>
    where
        I: IntoIterator<Item = &'a Type>,
    {
        self.require_in_call();
        assert!(
            self.expectations
                .borrow_mut()
                .expect_validate_caller_type
                .is_some(),
            "unexpected validate caller code"
        );

        let find_by_type = |typ| {
            (*ACTOR_TYPES)
                .iter()
                .find_map(|(cid, t)| if t == typ { Some(cid) } else { None })
                .cloned()
                .unwrap()
        };
        let types: Vec<Cid> = types.into_iter().map(find_by_type).collect();
        let expected_caller_type = self
            .expectations
            .borrow_mut()
            .expect_validate_caller_type
            .clone()
            .unwrap();
        assert_eq!(
            &types, &expected_caller_type,
            "unexpected validate caller code {types:?}, expected {expected_caller_type:?}"
        );

        for expected in &types {
            if &self.caller_type == expected {
                self.expectations.borrow_mut().expect_validate_caller_type = None;
                return Ok(());
            }
        }

        self.expectations.borrow_mut().expect_validate_caller_type = None;
        Err(
            actor_error!(forbidden; "caller type {:?} forbidden, allowed: {:?}",
                self.caller_type, types),
        )
    }

    fn validate_immediate_caller_not_type<'a, I>(&mut self, types: I) -> Result<(), ActorError>
    where
        I: IntoIterator<Item = &'a Type>,
    {
        self.require_in_call();

        // still requires the caller type to be set otherwise we cannot check against not type
        assert!(
            self.expectations
                .borrow_mut()
                .expect_validate_caller_not_type
                .is_some(),
            "unexpected validate caller code"
        );

        let find_by_type = |typ| {
            (*ACTOR_TYPES)
                .iter()
                .find_map(|(cid, t)| if t == typ { Some(cid) } else { None })
                .cloned()
                .unwrap()
        };
        let types: Vec<Cid> = types.into_iter().map(find_by_type).collect();

        let expect_validate_caller_not_type = self
            .expectations
            .borrow_mut()
            .expect_validate_caller_not_type
            .clone()
            .unwrap();

        let mut r = Ok(());
        for unexpected in &types {
            if !expect_validate_caller_not_type.contains(unexpected) {
                r = Err(actor_error!(forbidden; "caller type {:?} not expected", unexpected));
                break;
            }
        }

        self.expectations
            .borrow_mut()
            .expect_validate_caller_not_type = None;
        r
    }

    fn current_balance(&self) -> TokenAmount {
        self.require_in_call();
        self.balance.borrow().clone()
    }

    fn resolve_address(&self, address: &Address) -> Option<Address> {
        self.require_in_call();
        if address.protocol() == Protocol::ID {
            return Some(*address);
        }
        self.id_addresses.get(address).cloned()
    }

    fn get_actor_code_cid(&self, id: &ActorID) -> Option<Cid> {
        self.require_in_call();
        self.actor_code_cids.get(&Address::new_id(*id)).cloned()
    }

    fn create<T: Serialize>(&mut self, obj: &T) -> Result<(), ActorError> {
        if self.state.is_some() {
            return Err(actor_error!(illegal_state; "state already constructed"));
        }
        self.state = Some(self.store_put(obj));
        Ok(())
    }

    fn state<T: DeserializeOwned>(&self) -> Result<T, ActorError> {
        Ok(self.store_get(self.state.as_ref().unwrap()))
    }

    fn transaction<T, RT, F>(&mut self, f: F) -> Result<RT, ActorError>
    where
        T: Serialize + DeserializeOwned,
        F: FnOnce(&mut T, &mut Self) -> Result<RT, ActorError>,
    {
        if self.in_transaction {
            return Err(actor_error!(assertion_failed; "nested transaction"));
        }
        let mut read_only = self.state()?;
        self.in_transaction = true;
        let ret = f(&mut read_only, self);
        if ret.is_ok() {
            self.state = Some(self.store_put(&read_only));
        }
        self.in_transaction = false;
        ret
    }

    fn store(&self) -> &Rc<BS> {
        &self.store
    }

    fn send(
        &self,
        to: &Address,
        method: MethodNum,
        params: Option<IpldBlock>,
        value: TokenAmount,
    ) -> Result<Option<IpldBlock>, ActorError> {
        self.require_in_call();
        if self.in_transaction {
            return Err(actor_error!(assertion_failed; "side-effect within transaction"));
        }

        assert!(
            !self.expectations.borrow_mut().expect_sends.is_empty(),
            "unexpected message to: {to:?} method: {method:?}, value: {value:?}, params: {params:?}"
        );

        let expected_msg = self
            .expectations
            .borrow_mut()
            .expect_sends
            .pop_front()
            .unwrap();

        assert_eq!(expected_msg.to, *to);
        assert_eq!(expected_msg.method, method);
        assert_eq!(expected_msg.params, params);
        assert_eq!(expected_msg.value, value);

        {
            let mut balance = self.balance.borrow_mut();
            if value > *balance {
                return Err(ActorError::unchecked(
                    ExitCode::SYS_SENDER_STATE_INVALID,
                    format!(
                        "cannot send value: {:?} exceeds balance: {:?}",
                        value, *balance
                    ),
                ));
            }
            *balance -= value;
        }

        match expected_msg.exit_code {
            ExitCode::OK => Ok(expected_msg.send_return),
            x => Err(ActorError::unchecked(
                x,
                "Expected message Fail".to_string(),
            )),
        }
    }

    fn new_actor_address(&mut self) -> Result<Address, ActorError> {
        self.require_in_call();
        let ret = *self
            .new_actor_addr
            .as_ref()
            .expect("unexpected call to new actor address");
        self.new_actor_addr = None;
        Ok(ret)
    }

    fn create_actor(&mut self, code_id: Cid, actor_id: ActorID) -> Result<(), ActorError> {
        self.require_in_call();
        if self.in_transaction {
            return Err(actor_error!(assertion_failed; "side-effect within transaction"));
        }
        let expect_create_actor = self
            .expectations
            .borrow_mut()
            .expect_create_actor
            .take()
            .expect("unexpected call to create actor");

        assert!(expect_create_actor.code_id == code_id && expect_create_actor.actor_id == actor_id, "unexpected actor being created, expected code: {:?} address: {:?}, actual code: {:?} address: {:?}", expect_create_actor.code_id, expect_create_actor.actor_id, code_id, actor_id);
        Ok(())
    }

    fn delete_actor(&mut self, burn_unspent: bool) -> Result<(), ActorError> {
        self.require_in_call();
        if self.in_transaction {
            return Err(actor_error!(assertion_failed; "side-effect within transaction"));
        }
        let exp_act = self.expectations.borrow_mut().expect_delete_actor.take();
        if exp_act.is_none() {
            panic!("unexpected call to delete actor: {burn_unspent}");
        }
        if exp_act.unwrap() != burn_unspent {
            panic!(
                "attempt to delete wrong actor. Expected: {}, got: {}",
                exp_act.unwrap(),
                burn_unspent
            );
        }
        Ok(())
    }

    fn resolve_builtin_actor_type(&self, code_id: &Cid) -> Option<Type> {
        self.require_in_call();
        (*ACTOR_TYPES).get(code_id).cloned()
    }

    fn get_code_cid_for_type(&self, typ: Type) -> Cid {
        self.require_in_call();
        (*ACTOR_TYPES)
            .iter()
            .find_map(|(cid, t)| if *t == typ { Some(cid) } else { None })
            .cloned()
            .unwrap()
    }

    fn total_fil_circ_supply(&self) -> TokenAmount {
        self.circulating_supply.clone()
    }

    fn charge_gas(&mut self, _: &'static str, value: i64) {
        let mut exs = self.expectations.borrow_mut();
        assert!(
            !exs.expect_gas_charge.is_empty(),
            "unexpected gas charge {value:?}"
        );
        let expected = exs.expect_gas_charge.pop_front().unwrap();
        assert_eq!(
            expected, value,
            "expected gas charge {expected:?}, actual {value:?}"
        );
    }

    fn base_fee(&self) -> TokenAmount {
        self.base_fee.clone()
    }
}

impl<BS> Primitives for MockRuntime<BS> {
    fn verify_signature(
        &self,
        signature: &Signature,
        signer: &Address,
        plaintext: &[u8],
    ) -> anyhow::Result<()> {
        if self.expectations.borrow_mut().expect_verify_sigs.is_empty() {
            panic!(
                "Unexpected signature verification sig: {:?}, signer: {}, plaintext: {}",
                signature,
                signer,
                hex::encode(plaintext)
            );
        }
        let exp = self
            .expectations
            .borrow_mut()
            .expect_verify_sigs
            .pop_front();
        if let Some(exp) = exp {
            if exp.sig != *signature || exp.signer != *signer || &exp.plaintext[..] != plaintext {
                panic!(
                    "unexpected signature verification\n\
                    sig: {:?}, signer: {}, plaintext: {}\n\
                    expected sig: {:?}, signer: {}, plaintext: {}",
                    signature,
                    signer,
                    hex::encode(plaintext),
                    exp.sig,
                    exp.signer,
                    hex::encode(exp.plaintext)
                )
            }
            exp.result?
        } else {
            panic!(
                "unexpected syscall to verify signature: {:?}, signer: {}, plaintext: {}",
                signature,
                signer,
                hex::encode(plaintext)
            )
        }
        Ok(())
    }

    fn hash_blake2b(&self, data: &[u8]) -> [u8; 32] {
        (*self.hash_func)(data)
    }
}

pub fn blake2b_256(data: &[u8]) -> [u8; 32] {
    blake2b_simd::Params::new()
        .hash_length(32)
        .to_state()
        .update(data)
        .finalize()
        .as_bytes()
        .try_into()
        .unwrap()
}

// multihash library doesn't support poseidon hashing, so we fake it
#[derive(Clone, Copy, Debug, Eq, Multihash, PartialEq)]
#[mh(alloc_size = 64)]
enum MhCode {
    #[mh(code = 0xb401, hasher = multihash::Sha2_256)]
    PoseidonFake,
    #[mh(code = 0x1012, hasher = multihash::Sha2_256)]
    Sha256TruncPaddedFake,
}

fn make_cid(input: &[u8], prefix: u64, hash: MhCode) -> Cid {
    let hash = hash.digest(input);
    Cid::new_v1(prefix, hash)
}

pub fn make_cid_sha(input: &[u8], prefix: u64) -> Cid {
    make_cid(input, prefix, MhCode::Sha256TruncPaddedFake)
}

pub fn make_cid_poseidon(input: &[u8], prefix: u64) -> Cid {
    make_cid(input, prefix, MhCode::PoseidonFake)
}

pub fn make_piece_cid(input: &[u8]) -> Cid {
    make_cid_sha(input, FIL_COMMITMENT_UNSEALED)
}

pub fn make_sealed_cid(input: &[u8]) -> Cid {
    make_cid_poseidon(input, FIL_COMMITMENT_SEALED)
}

pub fn new_bls_addr(s: u8) -> Address {
    let seed = [s; 32];
    let mut rng: StdRng = SeedableRng::from_seed(seed);
    let mut key = [0u8; 48];
    rng.fill_bytes(&mut key);
    Address::new_bls(&key).unwrap()
}
