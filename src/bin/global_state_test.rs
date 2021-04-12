#![allow(dead_code)]
#![allow(clippy::upper_case_acronyms)]
#![allow(clippy::large_enum_variant)]

use anyhow::{anyhow, Result};
use rust_decimal::Decimal;
use serde_json::Value;
use state_keeper::circuit_test::{self, messages, types};
use state_keeper::state::{common, global_state};
//use std::cmp;
use std::ops::{Deref, DerefMut};
use std::collections::HashMap;
use std::fs;
use std::fs::File;
use std::io::{BufRead, BufReader, Lines, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

enum WrappedMessage {
    BALANCE(messages::BalanceMessage),
    TRADE(messages::TradeMessage),
    ORDER(messages::OrderMessage),
}

fn parse_msg(line: String) -> Result<WrappedMessage> {
    let v: Value = serde_json::from_str(&line)?;
    if let Value::String(typestr) = &v["type"] {
        let val = v["value"].clone();

        match typestr.as_str() {
            "BalanceMessage" => {
                let data = serde_json::from_value(val).map_err(|e| anyhow!("wrong balance: {}", e))?;
                Ok(WrappedMessage::BALANCE(data))
            }
            "OrderMessage" => {
                let data = serde_json::from_value(val).map_err(|e| anyhow!("wrong balance: {}", e))?;
                Ok(WrappedMessage::ORDER(data))
            }
            "TradeMessage" => {
                let data = serde_json::from_value(val).map_err(|e| anyhow!("wrong balance: {}", e))?;
                Ok(WrappedMessage::TRADE(data))
            }
            other => Err(anyhow!("unrecognized type field {}", other)),
        }
    } else {
        Err(anyhow!("missed or unexpected type field: {}", line))
    }
}

type PlaceOrderType = HashMap<u32, (u32, u64)>;
//index type?
#[derive(Debug)]
struct PlaceOrder {
    ordermapping : PlaceOrderType,
    place_bench : f32,
    spot_bench : f32,
}

impl Deref for PlaceOrder {
    type Target = PlaceOrderType;
    fn deref(&self) -> &Self::Target {
        &self.ordermapping
    }
}

impl DerefMut for PlaceOrder {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.ordermapping
    }
}

impl Default for PlaceOrder {
    fn default() -> Self {
        PlaceOrder {
            ordermapping: PlaceOrderType::new(),
            place_bench : 0.0,
            spot_bench : 0.0,
        }
    }
}

type PlaceAccountType = HashMap<u32, u32>;
//index type?
#[derive(Debug)]
struct PlaceAccount(PlaceAccountType);

impl Deref for PlaceAccount {
    type Target = PlaceAccountType;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for PlaceAccount {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

mod test_const {

    pub const NTXS: usize = 2;
    pub const BALANCELEVELS: usize = 2;
    pub const ORDERLEVELS: usize = 7;
    pub const ACCOUNTLEVELS: usize = 2;
    pub const MAXORDERNUM: usize = 2usize.pow(ORDERLEVELS as u32);
    pub const MAXACCOUNTNUM: usize = 2usize.pow(ACCOUNTLEVELS as u32);
    pub const MAXTOKENNUM: usize = 2usize.pow(BALANCELEVELS as u32);
    pub const VERBOSE: bool = false;

    pub fn token_id(token_name: &str) -> u32 {
        match token_name {
            "ETH" => 0,
            "USDT" => 1,
            _ => unreachable!(),
        }
    }

    pub fn prec(token_id: u32) -> u32 {
        match token_id {
            0 | 1 => 6,
            _ => unreachable!(),
        }
    }
}


//make ad-hoc transform in account_id
impl PlaceAccount {

    fn obtain_place_id(&mut self, state: &mut global_state::GlobalState, 
        account_id : u32) -> u32{

        match self.get(&account_id) {
            Some(pl_id) => *pl_id,
            None => {
                let uid = state.create_new_account(1);
                self.insert(account_id, uid);
                if test_const::VERBOSE {
                    println!(
                        "global account id {} to user account id {}",
                        uid, account_id
                    );
                }                
                uid
            }
        }
    }

    pub fn transform_trade(&mut self, state: &mut global_state::GlobalState, 
        mut trade: messages::TradeMessage) -> messages::TradeMessage {
        
        trade.ask_user_id = self.obtain_place_id(state, trade.ask_user_id);
        trade.bid_user_id = self.obtain_place_id(state, trade.bid_user_id);

        trade
    }

    pub fn handle_deposit(&mut self, state: &mut global_state::GlobalState, 
        mut deposit: messages::BalanceMessage) {
        //integrate the sanity check here ...
        deposit.user_id = self.obtain_place_id(state, deposit.user_id);

        assert!(!deposit.change.is_sign_negative(), "only support deposit now");
    
        let token_id = test_const::token_id(&deposit.asset);
    
        let balance_before = deposit.balance - deposit.change;
        assert!(!balance_before.is_sign_negative(), "invalid balance {:?}", deposit);
    
        let expected_balance_before = state.get_token_balance(deposit.user_id, token_id);
        assert_eq!(
            expected_balance_before,
            types::number_to_integer(&balance_before, test_const::prec(token_id))
        );
    
        state.deposit_to_old(common::DepositToOldTx {
            token_id,
            account_id: deposit.user_id,
            amount: types::number_to_integer(&deposit.change, test_const::prec(token_id)),
        });
    }
    

}

#[derive(Clone, Copy)]
struct TokenIdPair(u32, u32);
/*
impl TokenIdPair {
    fn swap(&mut self) {
        let tmp = self.1;
        self.1 = self.0;
        self.0 = tmp;
    }
}
*/
#[derive(Clone, Copy)]
struct TokenPair<'c>(&'c str, &'c str);

struct OrderState<'c> {
    origin: &'c messages::VerboseOrderState,
    side: &'static str,
    token_sell: u32,
    token_buy: u32,
    total_sell: Decimal,
    total_buy: Decimal,
    filled_sell: Decimal,
    filled_buy: Decimal,

    order_id: u32,
    account_id: u32,
    role: messages::MarketRole,
}

struct OrderStateTag {
    id: u64,
    account_id: u32,
    role: messages::MarketRole,
}

impl<'c> From<&'c str> for TokenPair<'c> {
    fn from(origin: &'c str) -> Self {
        let mut assets = origin.split('_');
        let asset_1 = assets.next().unwrap();
        let asset_2 = assets.next().unwrap();
        TokenPair(asset_1, asset_2)
    }
}

impl<'c> From<TokenPair<'c>> for TokenIdPair {
    fn from(origin: TokenPair<'c>) -> Self {
        TokenIdPair(test_const::token_id(origin.0), test_const::token_id(origin.1))
    }
}

impl<'c> OrderState<'c> {
    fn parse(
        origin: &'c messages::VerboseOrderState,
        id_pair: TokenIdPair,
        _token_pair: TokenPair<'c>,
        side: &'static str,
        trade: &messages::TradeMessage,
    ) -> Self {
        match side {
            "ASK" => OrderState {
                origin,
                side,
                //status: 0,
                token_sell: id_pair.0,
                token_buy: id_pair.1,
                total_sell: origin.amount,
                total_buy: origin.amount * origin.price,
                filled_sell: origin.finished_base,
                filled_buy: origin.finished_quote,
                order_id: trade.ask_order_id as u32,
                account_id: trade.ask_user_id,
                role: trade.ask_role,
            },
            "BID" => OrderState {
                origin,
                side,
                //status: 0,
                token_sell: id_pair.1,
                token_buy: id_pair.0,
                total_sell: origin.amount * origin.price,
                total_buy: origin.amount,
                filled_sell: origin.finished_quote,
                filled_buy: origin.finished_base,
                order_id: trade.bid_order_id as u32,
                account_id: trade.bid_user_id,
                role: trade.bid_role,
            },
            _ => unreachable!(),
        }
    }
    fn place_order_tx(&self) -> common::PlaceOrderTx {
        common::PlaceOrderTx {
            order_id: self.order_id,
            account_id: self.account_id,
            token_id_sell: self.token_sell,
            token_id_buy: self.token_buy,
            amount_sell: types::number_to_integer(&self.total_sell, test_const::prec(self.token_sell)),
            amount_buy: types::number_to_integer(&self.total_buy, test_const::prec(self.token_buy)),
        }
    }
}

impl<'c> From<OrderState<'c>> for common::Order {
    fn from(origin: OrderState<'c>) -> Self {
        common::Order {
            order_id: types::u32_to_fr(origin.order_id),
            //status: types::u32_to_fr(origin.status),
            tokenbuy: types::u32_to_fr(origin.token_buy),
            tokensell: types::u32_to_fr(origin.token_sell),
            filled_sell: types::number_to_integer(&origin.filled_sell, test_const::prec(origin.token_sell)),
            filled_buy: types::number_to_integer(&origin.filled_buy, test_const::prec(origin.token_buy)),
            total_sell: types::number_to_integer(&origin.total_sell, test_const::prec(origin.token_sell)),
            total_buy: types::number_to_integer(&origin.total_buy, test_const::prec(origin.token_buy)),
        }
    }
}
impl<'c> std::cmp::PartialOrd for OrderState<'c> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<'c> std::cmp::PartialEq for OrderState<'c> {
    fn eq(&self, other: &Self) -> bool {
        self.order_id == other.order_id
    }
}

impl<'c> std::cmp::Eq for OrderState<'c> {}

impl<'c> std::cmp::Ord for OrderState<'c> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.order_id.cmp(&other.order_id)
    }
}

#[derive(PartialEq, Debug)]
struct CommonBalanceState {
    bid_user_base: types::Fr,
    bid_user_quote: types::Fr,
    ask_user_base: types::Fr,
    ask_user_quote: types::Fr,
}

impl CommonBalanceState {
    fn parse(origin: &messages::VerboseBalanceState, id_pair: TokenIdPair) -> Self {
        let base_id = id_pair.0;
        let quote_id = id_pair.1;

        CommonBalanceState {
            bid_user_base: types::number_to_integer(&origin.bid_user_base, test_const::prec(base_id)),
            bid_user_quote: types::number_to_integer(&origin.bid_user_quote, test_const::prec(quote_id)),
            ask_user_base: types::number_to_integer(&origin.ask_user_base, test_const::prec(base_id)),
            ask_user_quote: types::number_to_integer(&origin.ask_user_quote, test_const::prec(quote_id)),
        }
    }

    fn build_local(state: &global_state::GlobalState, bid_id: u32, ask_id: u32, id_pair: TokenIdPair) -> Self {
        let base_id = id_pair.0;
        let quote_id = id_pair.1;

        CommonBalanceState {
            bid_user_base: state.get_token_balance(bid_id, base_id),
            bid_user_quote: state.get_token_balance(bid_id, quote_id),
            ask_user_base: state.get_token_balance(ask_id, base_id),
            ask_user_quote: state.get_token_balance(ask_id, quote_id),
        }
    }
}

fn assert_balance_state(
    balance_state: &messages::VerboseBalanceState,
    state: &global_state::GlobalState,
    bid_id: u32,
    ask_id: u32,
    id_pair: TokenIdPair,
) {
    let local_balance = CommonBalanceState::build_local(state, bid_id, ask_id, id_pair);
    let parsed_state = CommonBalanceState::parse(balance_state, id_pair);
    assert_eq!(local_balance, parsed_state);
}

impl PlaceOrder {

    fn take_bench(&mut self) -> (f32, f32) {
        let ret = (self.place_bench, self.spot_bench);
        self.place_bench = 0.0;
        self.spot_bench = 0.0;
        ret
    }

    fn assert_order_state<'c>(&self, state: &global_state::GlobalState, ask_order_state: OrderState<'c>, bid_order_state: OrderState<'c>) {
        let ask_order_local = state
            .get_account_order_by_id(ask_order_state.account_id, ask_order_state.order_id)
            .unwrap();
        assert_eq!(ask_order_local, common::Order::from(ask_order_state));

        let bid_order_local = state
            .get_account_order_by_id(bid_order_state.account_id, bid_order_state.order_id)
            .unwrap();
        assert_eq!(bid_order_local, common::Order::from(bid_order_state));
    }

    fn trade_into_spot_tx(&self, trade: &messages::TradeMessage) -> common::SpotTradeTx {
        //allow information can be obtained from trade
        let id_pair = TokenIdPair::from(TokenPair::from(trade.market.as_str()));

        match trade.ask_role {
            messages::MarketRole::MAKER => common::SpotTradeTx {
                order1_account_id: trade.ask_user_id,
                order2_account_id: trade.bid_user_id,
                token_id_1to2: id_pair.0,
                token_id_2to1: id_pair.1,
                amount_1to2: types::number_to_integer(&trade.amount, test_const::prec(id_pair.0)),
                amount_2to1: types::number_to_integer(&trade.quote_amount, test_const::prec(id_pair.1)),
                order1_id: trade.ask_order_id as u32,
                order2_id: trade.bid_order_id as u32,
            },
            messages::MarketRole::TAKER => common::SpotTradeTx {
                order1_account_id: trade.bid_user_id,
                order2_account_id: trade.ask_user_id,
                token_id_1to2: id_pair.1,
                token_id_2to1: id_pair.0,
                amount_1to2: types::number_to_integer(&trade.quote_amount, test_const::prec(id_pair.1)),
                amount_2to1: types::number_to_integer(&trade.amount, test_const::prec(id_pair.0)),
                order1_id: trade.bid_order_id as u32,
                order2_id: trade.ask_order_id as u32,
            },
        }
    }

    fn handle_trade(&mut self, state: &mut global_state::GlobalState, trade: messages::TradeMessage) {
        let token_pair = TokenPair::from(trade.market.as_str());
        let id_pair = TokenIdPair::from(token_pair);

        let ask_order_state_before: OrderState = OrderState::parse(&trade.state_before.ask_order_state, id_pair, token_pair, "ASK", &trade);

        let bid_order_state_before: OrderState = OrderState::parse(&trade.state_before.bid_order_state, id_pair, token_pair, "BID", &trade);

        //this field is not used yet ...
        let ask_order_state_after: OrderState = OrderState::parse(&trade.state_after.ask_order_state, id_pair, token_pair, "ASK", &trade);

        let bid_order_state_after: OrderState = OrderState::parse(&trade.state_after.bid_order_state, id_pair, token_pair, "BID", &trade);

        //seems we do not need to use map/zip liket the ts code because the suitable order_id has been embedded
        //into the tag.id field
        let mut put_states = vec![&ask_order_state_before, &bid_order_state_before];
        put_states.sort();

        let mut timing = Instant::now();
        for order_state in put_states.into_iter() {
            if !self.contains_key(&order_state.order_id) {
                //why the returning order id is u32?
                // in fact the GlobalState should not expose "inner idx/pos" to caller
                // we'd better handle this inside GlobalState later
                let new_order_pos = state.place_order(order_state.place_order_tx());
                self.insert(order_state.order_id, (order_state.account_id, new_order_pos as u64));
                if test_const::VERBOSE {
                    println!(
                        "global order id {} to user order id ({},{})",
                        order_state.order_id, order_state.account_id, new_order_pos
                    );
                }
            } else if test_const::VERBOSE {
                println!("skip put order {}", order_state.order_id);
            }
        }
        self.place_bench += timing.elapsed().as_secs_f32();

        timing = Instant::now();

        assert_balance_state(
            &trade.state_before.balance,
            state,
            bid_order_state_before.account_id,
            ask_order_state_before.account_id,
            id_pair,
        );
        self.assert_order_state(state, ask_order_state_before, bid_order_state_before);

        state.spot_trade(self.trade_into_spot_tx(&trade));
        self.spot_bench += timing.elapsed().as_secs_f32();

        assert_balance_state(
            &trade.state_after.balance,
            state,
            bid_order_state_after.account_id,
            ask_order_state_after.account_id,
            id_pair,
        );
        self.assert_order_state(state, ask_order_state_after, bid_order_state_after);
    }
}

//if we use nightly build, we are able to use bench test ...
fn bench_global_state(circuit_repo: &Path) -> Result<Vec<common::L2Block>>{
    let test_dir = circuit_repo.join("test").join("testdata");
    let file = File::open(test_dir.join("msgs_float.jsonl"))?;

    let messages: Vec<WrappedMessage> = BufReader::new(file).lines()
        .map(Result::unwrap)
        .map(parse_msg)
        .map(Result::unwrap)
        .filter(|msg|{
            match msg {
                WrappedMessage::BALANCE(_) |
                WrappedMessage::TRADE(_) => true,
                _ => false
            }
        })
        .collect();

    println!("prepare bench: {} records", messages.len());
    
    //use custom states
    let mut state = global_state::GlobalState::new(
        10,//test_const::BALANCELEVELS,
        10,//test_const::ORDERLEVELS,
        10,//test_const::ACCOUNTLEVELS,
        test_const::NTXS,
        false,
    );

    //amplify the records: in each iter we run records on a group of new accounts
    let mut timing = Instant::now();
    for i in 1..51 {
        let mut place_order = PlaceOrder::default();
        let mut place_account = PlaceAccount(PlaceAccountType::new());
        

        for msg in messages.iter() {
            match msg {
                WrappedMessage::BALANCE(balance) => {
                    place_account.handle_deposit(&mut state, balance.clone());
                },
                WrappedMessage::TRADE(trade) => {
                    let trade = place_account.transform_trade(&mut state, trade.clone());
                    place_order.handle_trade(&mut state, trade);
                },
                _ => unreachable!(),
            }
        }
        if i % 10 == 0 {
            let total = timing.elapsed().as_secs_f32();
            let (plact_t, spot_t) = place_order.take_bench();
            println!("{}th 10 iters in {}s: place {}%, spot {}%", i / 10, total, 
                plact_t * 100.0 / total, spot_t * 100.0 / total);
            timing = Instant::now();
        }
        
    }

    Ok(state.take_blocks())
}

fn replay_msgs(circuit_repo: &Path) -> Result<(Vec<common::L2Block>, types::CircuitSource)> {
    let test_dir = circuit_repo.join("test").join("testdata");
    let file = File::open(test_dir.join("msgs_float.jsonl"))?;

    let lns: Lines<BufReader<File>> = BufReader::new(file).lines();

    let mut state = global_state::GlobalState::new(
        test_const::BALANCELEVELS,
        test_const::ORDERLEVELS,
        test_const::ACCOUNTLEVELS,
        test_const::NTXS,
        test_const::VERBOSE,
    );

    println!("genesis root {}", state.root());

    let mut place_order = PlaceOrder::default();
    let mut place_account = PlaceAccount(PlaceAccountType::new());
/*
    for _ in 0..test_const::MAXACCOUNTNUM {
        state.create_new_account(1);
    }
*/
    for line in lns {
        let msg = line.map(parse_msg)??;
        match msg {
            WrappedMessage::BALANCE(balance) => {
                place_account.handle_deposit(&mut state, balance);
            }
            WrappedMessage::TRADE(trade) => {
                let trade = place_account.transform_trade(&mut state, trade);
                let trade_id = trade.id;
                place_order.handle_trade(&mut state, trade);
                println!("trade {} test done", trade_id);
            }
            _ => {
                //other msg is omitted
            }
        }
    }

    state.flush_with_nop();

    let component = types::CircuitSource {
        src: String::from("src/block.circom"),
        main: format!(
            "Block({}, {}, {}, {})",
            test_const::NTXS,
            test_const::BALANCELEVELS,
            test_const::ORDERLEVELS,
            test_const::ACCOUNTLEVELS
        ),
    };

    Ok((state.take_blocks(), component))
}

//just grap from export_circuit_test.rs ...
fn write_circuit(circuit_repo: &Path, test_dir: &Path, source: &circuit_test::types::CircuitSource) -> Result<PathBuf> {
    let circuit_name = circuit_test::types::format_circuit_name(source.main.as_str());
    let circuit_dir = test_dir.join(circuit_name);

    fs::create_dir_all(circuit_dir.clone())?;

    let circuit_file = circuit_dir.join("circuit.circom");

    //in os beyond UNIX the slash in source wolud not be considerred as separator
    //so we need to convert them explicity
    let src_path: PathBuf = source.src.split('/').collect();

    let file_content = format!(
        "include \"{}\";\ncomponent main = {}",
        circuit_repo.join(src_path).to_str().unwrap(),
        source.main
    );
    let mut f = File::create(circuit_file)?;
    f.write_all(&file_content.as_bytes())?;
    Ok(circuit_dir)
}

fn write_input(input_dir: &Path, block: common::L2Block) -> Result<()> {
    fs::create_dir_all(input_dir)?;
    let input_f = File::create(input_dir.join("input.json"))?;
    serde_json::to_writer_pretty(input_f, &types::L2BlockSerde::from(block))?;
    let output_f = File::create(input_dir.join("output.json"))?;
    //TODO: no output?
    serde_json::to_writer_pretty(output_f, &serde_json::Value::Object(Default::default()))?;

    Ok(())
}

fn export_circuit_and_testdata(
    circuit_repo: &Path,
    blocks: Vec<common::L2Block>,
    source: circuit_test::types::CircuitSource,
) -> Result<PathBuf> {
    let test_dir = circuit_repo.join("testdata");
    let circuit_dir = write_circuit(circuit_repo, &test_dir, &source)?;

    for (blki, blk) in blocks.into_iter().enumerate() {
        let input_dir = circuit_dir.join(format!("{:04}", blki));
        write_input(&input_dir, blk)?;
        //println!("{}", serde_json::to_string_pretty(&types::L2BlockSerde::from(blk)).unwrap());
    }

    Ok(circuit_dir)
}


fn test_bench() -> Result<()> {
    let circuit_repo = fs::canonicalize(PathBuf::from("../circuits")).expect("invalid circuits repo path");

    let timing = Instant::now();
    let blocks = bench_global_state(&circuit_repo)?;
    println!(
        "bench for {} blocks (TPS: {})",
        blocks.len(),
        (test_const::NTXS * blocks.len()) as f32 / timing.elapsed().as_secs_f32()
    );

    Ok(())
}


fn test_all() -> Result<()> {
    let circuit_repo = fs::canonicalize(PathBuf::from("../circuits")).expect("invalid circuits repo path");

    let timing = Instant::now();
    let (blocks, components) = replay_msgs(&circuit_repo)?;
    println!(
        "genesis {} blocks (TPS: {})",
        blocks.len(),
        (test_const::NTXS * blocks.len()) as f32 / timing.elapsed().as_secs_f32()
    );

    let circuit_dir = export_circuit_and_testdata(&circuit_repo, blocks, components)?;

    println!("TODO: test circuit dir {}", circuit_dir.to_str().unwrap());

    Ok(())
}

fn main() {
    match test_all() {
        Ok(_) => {}
        Err(e) => {
            eprintln!("{:#?}", e);
            std::process::exit(1);
        }
    }
    #[cfg(feature = "bench_global_state")]
    test_bench().expect("bench ok");
}
