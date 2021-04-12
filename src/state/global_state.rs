#![allow(clippy::field_reassign_with_default)]

// from https://github1s.com/Fluidex/circuits/blob/HEAD/test/global_state.ts

use super::codec::place_order_tx_data;
use super::common::{tx_detail_idx, AccountState, DepositToOldTx, L2Block, Order, PlaceOrderTx, RawTx, SpotTradeTx, TxType, TX_LENGTH};
use super::merkle_tree::{empty_tree_root, Tree};
use super::types::{field_to_u32, u32_to_fr, Fr};
use ff::Field;
use fnv::FnvHashMap;
use std::collections::BTreeMap;

pub struct StateProof {
    leaf: Fr,
    root: Fr,
    balance_root: Fr,
    order_root: Fr,
    balance_path: Vec<[Fr; 1]>,
    account_path: Vec<[Fr; 1]>,
}

// TODO: change to snake_case
// TODO: too many unwrap here
pub struct GlobalState {
    n_tx: usize,
    balance_levels: usize,
    order_levels: usize,
    account_levels: usize,
    account_tree: Tree,
    // idx to balanceTree
    balance_trees: FnvHashMap<u32, Tree>,
    order_trees: FnvHashMap<u32, Tree>,
    order_map: FnvHashMap<u32, BTreeMap<u32, Order>>,
    // (user, order_id) -> order_pos
    order_id_to_pos: FnvHashMap<(u32, u32), u32>,
    accounts: FnvHashMap<u32, AccountState>,
    buffered_txs: Vec<RawTx>,
    buffered_blocks: Vec<L2Block>,
    default_balance_root: Fr,
    default_order_leaf: Fr,
    default_order_root: Fr,
    default_account_leaf: Fr,
    // here we use idx to indicate the location in the map
    // idx must be less than 2**n
    // mean while id means the global incremental seq/nouce
    next_order_idxs: FnvHashMap<u32, u32>,
    max_order_num_per_user: u32,
    verbose: bool,
}

impl GlobalState {
    pub fn new(balance_levels: usize, order_levels: usize, account_levels: usize, n_tx: usize, verbose: bool) -> Self {
        Tree::print_config();
        let default_balance_root = empty_tree_root(balance_levels, Fr::zero());
        let default_order_leaf = Order::default().hash();
        let dummy_order_tree = Tree::new(order_levels, default_order_leaf);
        let default_order_root = dummy_order_tree.get_root();
        let default_account_leaf = AccountState::empty(default_balance_root, default_order_root).hash();
        let max_order_num_per_user = dummy_order_tree.max_leaf_num();
        Self {
            balance_levels,
            order_levels,
            account_levels,
            default_balance_root,
            default_order_leaf,
            default_order_root,
            // default_account_leaf depends on default_order_root and default_balance_root
            default_account_leaf,
            account_tree: Tree::new(account_levels, default_account_leaf), // Tree<account_hash>
            balance_trees: FnvHashMap::default(),                          // FnvHashMap[account_id]balance_tree
            order_trees: FnvHashMap::default(),                            // FnvHashMap[account_id]order_tree
            order_map: FnvHashMap::default(),
            order_id_to_pos: FnvHashMap::default(),
            accounts: FnvHashMap::default(), // FnvHashMap[account_id]acount_state
            buffered_txs: Vec::new(),
            buffered_blocks: Vec::new(),
            next_order_idxs: FnvHashMap::default(),
            max_order_num_per_user,
            n_tx,
            verbose,
        }
    }
    pub fn root(&self) -> Fr {
        self.account_tree.get_root()
    }
    fn recalculate_from_account_state(&mut self, account_id: u32) {
        self.account_tree
            .set_value(account_id, self.accounts.get(&account_id).unwrap().hash());
    }
    fn recalculate_from_balance_tree(&mut self, account_id: u32) {
        self.accounts.get_mut(&account_id).unwrap().balance_root = self.balance_trees.get(&account_id).unwrap().get_root();
        self.recalculate_from_account_state(account_id);
    }
    fn recalculate_from_order_tree(&mut self, account_id: u32) {
        self.accounts.get_mut(&account_id).unwrap().order_root = self.order_trees.get(&account_id).unwrap().get_root();
        self.recalculate_from_account_state(account_id);
    }
    /*
    pub fn setAccountKey(&mut self, account_id: Fr, account: Account) {
      //println!("setAccountKey", account_id);
      self.accounts.get(account_id).updateAccountKey(account);
      self.recalculate_from_account_state(account_id);
    }
    pub fn setAccountL2Addr(&mut self, account_id: Fr, sign, ay, eth_addr) {
      self.accounts.get(account_id).update_l2_addr(sign, ay, eth_addr);
      self.recalculate_from_account_state(account_id);
    }
    */
    // TODO: we should change account_id to u32 later?
    pub fn set_account_nonce(&mut self, account_id: u32, nonce: Fr) {
        self.accounts.get_mut(&account_id).unwrap().update_nonce(nonce);
        self.recalculate_from_account_state(account_id);
    }
    // self function should only be used in tests for convenience
    pub fn set_account_order_root(&mut self, account_id: u32, order_root: Fr) {
        self.accounts.get_mut(&account_id).unwrap().update_order_root(order_root);
        self.recalculate_from_account_state(account_id);
    }
    fn increase_nonce(&mut self, account_id: u32) {
        let mut nonce = self.accounts.get(&account_id).unwrap().nonce;
        nonce.add_assign(&Fr::one());
        //println!("oldNonce", oldNonce);
        self.set_account_nonce(account_id, nonce);
    }
    pub fn get_account(&self, account_id: u32) -> AccountState {
        *self.accounts.get(&account_id).unwrap()
    }
    fn get_next_order_idx_for_user(&self, account_id: u32) -> (u32, bool /*is_overwrite*/) {
        let cur_idx = *self.next_order_idxs.get(&account_id).unwrap();
        let leaf_num = self.order_trees.get(&account_id).unwrap().max_leaf_num();
        for offset in 0..leaf_num {
            let order_idx = (cur_idx + offset) % leaf_num;
            match self.get_account_order_by_pos(account_id, order_idx) {
                None => return (order_idx, false),
                Some(order) => {
                    // so there is already an order in this location
                    if order.is_filled() {
                        return (order_idx, true);
                    }
                    // the order is active
                    continue;
                }
            }
        }
        // we cannot find a order slot that can be overwrite
        panic!("order tree full for user {}", account_id);
    }
    pub fn create_new_account(&mut self, next_order_id: u32) -> u32 {
        let account_id = self.balance_trees.len() as u32;
        if account_id >= 2u32.pow(self.account_levels as u32) {
            panic!("account_id {} overflows for account_levels {}", account_id, self.account_levels);
        }

        let account_state = AccountState::empty(self.default_balance_root, self.default_order_root);
        self.accounts.insert(account_id, account_state);
        self.balance_trees.insert(account_id, Tree::new(self.balance_levels, Fr::zero()));
        self.order_trees
            .insert(account_id, Tree::new(self.order_levels, self.default_order_leaf));
        self.order_map.insert(account_id, BTreeMap::<u32, Order>::default());
        self.account_tree.set_value(account_id, self.default_account_leaf);
        self.next_order_idxs.insert(account_id, next_order_id);
        //println!("add account", account_id);
        account_id
    }
    pub fn get_order_pos_by_id(&self, account_id: u32, order_id: u32) -> u32 {
        *self.order_id_to_pos.get(&(account_id, order_id)).unwrap()
    }

    pub fn set_account_order(&mut self, account_id: u32, order_pos: u32, order: Order) {
        assert!(self.order_trees.contains_key(&account_id), "set_account_order");
        if order_pos >= 2u32.pow(self.order_levels as u32) {
            panic!("order_pos {} invalid for order_levels {}", order_pos, self.order_levels);
        }
        self.order_trees.get_mut(&account_id).unwrap().set_value(order_pos, order.hash());
        self.order_map.get_mut(&account_id).unwrap().insert(order_pos, order);
        // TODO: better type here...
        let order_id: u32 = field_to_u32(&order.order_id);
        self.order_id_to_pos.insert((account_id, order_id), order_pos);
        self.recalculate_from_order_tree(account_id);
    }
    pub fn create_new_order(&mut self, tx: &PlaceOrderTx) -> (u32, Order) {
        let (order_pos, _is_overwrite) = self.get_next_order_idx_for_user(tx.account_id);
        //if order_id >= 2u32.pow(self.order_levels as u32) {
        //    panic!("order_id {} overflows for order_levels {}", order_id, self.order_levels);
        //}
        let old_order = self.get_account_order_by_pos(tx.account_id, order_pos).unwrap_or_default();
        let order = Order {
            order_id: u32_to_fr(tx.order_id),
            tokenbuy: u32_to_fr(tx.token_id_buy),
            tokensell: u32_to_fr(tx.token_id_sell),
            filled_sell: Fr::zero(),
            filled_buy: Fr::zero(),
            total_sell: tx.amount_sell,
            total_buy: tx.amount_buy,
        };
        self.set_account_order(tx.account_id, order_pos, order);
        let next_order_idx = (order_pos + 1) % self.max_order_num_per_user;
        self.next_order_idxs.insert(tx.account_id, next_order_idx);
        (order_pos, old_order)
    }

    pub fn get_token_balance(&self, account_id: u32, token_id: u32) -> Fr {
        self.balance_trees.get(&account_id).unwrap().get_leaf(token_id)
    }
    pub fn set_token_balance(&mut self, account_id: u32, token_id: u32, balance: Fr) {
        assert!(self.balance_trees.contains_key(&account_id), "set_token_balance");
        self.balance_trees.get_mut(&account_id).unwrap().set_value(token_id, balance);
        self.recalculate_from_balance_tree(account_id);
    }
    fn get_account_order_by_pos(&self, account_id: u32, order_pos: u32) -> Option<Order> {
        self.order_map.get(&account_id).unwrap().get(&order_pos).cloned()
    }
    pub fn get_account_order_by_id(&self, account_id: u32, order_id: u32) -> Option<Order> {
        // TODO: make order_pos Option<>?
        let order_pos = self.get_order_pos_by_id(account_id, order_id);
        self.get_account_order_by_pos(account_id, order_pos)
    }

    pub fn trivial_order_path_elements(&self) -> Vec<[Fr; 1]> {
        Tree::new(self.order_levels, Fr::zero()).get_proof(0).path_elements
    }

    pub fn state_proof(&self, account_id: u32, token_id: u32) -> StateProof {
        let balance_proof = self.balance_trees.get(&account_id).unwrap().get_proof(token_id);
        let order_root = self.order_trees.get(&account_id).unwrap().get_root();
        let account_proof = self.account_tree.get_proof(account_id);
        //assert!(accountLeaf == balance_root, "state_proof");
        StateProof {
            leaf: balance_proof.leaf,
            root: account_proof.root,
            balance_root: balance_proof.root,
            order_root,
            balance_path: balance_proof.path_elements,
            account_path: account_proof.path_elements,
        }
    }
    pub fn get_l1_addr(&self, account_id: u32) -> Fr {
        return self.accounts.get(&account_id).unwrap().eth_addr;
    }
    pub fn forge_with_txs(&self, buffered_txs: &[RawTx]) -> L2Block {
        assert!(buffered_txs.len() == self.n_tx, "invalid txs len");
        let txs_type = buffered_txs.iter().map(|tx| tx.tx_type).collect();
        let encoded_txs = buffered_txs.iter().map(|tx| tx.payload.clone()).collect();
        let balance_path_elements = buffered_txs
            .iter()
            .map(|tx| {
                [
                    tx.balance_path0.clone(),
                    tx.balance_path1.clone(),
                    tx.balance_path2.clone(),
                    tx.balance_path3.clone(),
                ]
            })
            .collect();
        let order_path_elements = buffered_txs
            .iter()
            .map(|tx| [tx.order_path0.clone(), tx.order_path1.clone()])
            .collect();
        let order_roots = buffered_txs.iter().map(|tx| [tx.order_root0, tx.order_root1]).collect();
        let account_path_elements = buffered_txs
            .iter()
            .map(|tx| [tx.account_path0.clone(), tx.account_path1.clone()])
            .collect();
        let old_account_roots = buffered_txs.iter().map(|tx| tx.root_before).collect();
        let new_account_roots = buffered_txs.iter().map(|tx| tx.root_after).collect();
        L2Block {
            txs_type,
            encoded_txs,
            balance_path_elements,
            order_path_elements,
            account_path_elements,
            order_roots,
            old_account_roots,
            new_account_roots,
        }
    }
    pub fn forge(&mut self) -> L2Block {
        self.flush_with_nop();
        self.forge_with_txs(&self.buffered_txs)
    }
    pub fn forge_all_l2_blocks(&mut self) -> Vec<L2Block> {
        self.buffered_blocks.clone()
    }
    pub fn add_raw_tx(&mut self, raw_tx: RawTx) {
        self.buffered_txs.push(raw_tx);
        if self.buffered_txs.len() % self.n_tx == 0 {
            // forge next block, using last n_tx txs
            let txs = &self.buffered_txs[(self.buffered_txs.len() - self.n_tx)..];
            let block = self.forge_with_txs(txs);
            self.buffered_blocks.push(block);
            assert!(
                self.buffered_blocks.len() * self.n_tx == self.buffered_txs.len(),
                "invalid block num"
            );
            if self.verbose {
                println!("forge block {} done", self.buffered_blocks.len() - 1);
            }
        }
    }
    pub fn get_buffered_blocks(&self) -> &[L2Block] {
        self.buffered_blocks.as_slice()
    }
    pub fn take_blocks(self) -> Vec<L2Block> {
        self.buffered_blocks
    }

    /*
    DepositToNew(tx: DepositToNewTx) {
      assert!(self.accounts.get(tx.account_id).eth_addr == 0n, "DepositToNew");
      let proof = self.state_proof(tx.account_id, tx.token_id);
      // first, generate the tx
      let encoded_tx: Array<Fr> = new Array(Txlen());
      encoded_tx.fill(0n, 0, Txlen());
      encoded_tx[tx_detail_idx::TOKEN_ID] = Scalar.e(tx.token_id);
      encoded_tx[tx_detail_idx::AMOUNT] = tx.amount;
      encoded_tx[tx_detail_idx::ACCOUNT_ID2] = Scalar.e(tx.account_id);
      encoded_tx[tx_detail_idx::ETH_ADDR2] = tx.eth_addr;
      encoded_tx[tx_detail_idx::SIGN2] = Scalar.e(tx.sign);
      encoded_tx[tx_detail_idx::AY2] = tx.ay;
      let raw_tx: RawTx = {
        tx_type: TxType.DepositToNew,
        payload: encoded_tx,
        balance_path0: proof.balance_path,
        balance_path1: proof.balance_path,
        balance_path2: proof.balance_path,
        balance_path3: proof.balance_path,
        order_path0: self.trivial_order_path_elements(),
        order_path1: self.trivial_order_path_elements(),
        order_root0: self.default_order_root,
        order_root1: self.default_order_root,
        account_path0: proof.account_path,
        account_path1: proof.account_path,
        root_before: proof.root,
        root_after: 0n,
      };

      // then update global state
      self.set_token_balance(tx.account_id, tx.token_id, tx.amount);
      self.setAccountL2Addr(tx.account_id, tx.sign, tx.ay, tx.eth_addr);
      raw_tx.root_after = self.root();
      self.add_raw_tx(raw_tx);
    }
    */
    pub fn deposit_to_old(&mut self, tx: DepositToOldTx) {
        //assert!(self.accounts.get(tx.account_id).eth_addr != 0n, "deposit_to_old");
        let proof = self.state_proof(tx.account_id, tx.token_id);
        // first, generate the tx

        let mut encoded_tx = [Fr::zero(); TX_LENGTH];
        encoded_tx[tx_detail_idx::TOKEN_ID] = u32_to_fr(tx.token_id);
        encoded_tx[tx_detail_idx::AMOUNT] = tx.amount;
        encoded_tx[tx_detail_idx::ACCOUNT_ID2] = u32_to_fr(tx.account_id);
        let old_balance = self.get_token_balance(tx.account_id, tx.token_id);
        encoded_tx[tx_detail_idx::BALANCE2] = old_balance;
        encoded_tx[tx_detail_idx::NONCE2] = self.accounts.get(&tx.account_id).unwrap().nonce;
        let acc = self.accounts.get(&tx.account_id).unwrap();
        encoded_tx[tx_detail_idx::ETH_ADDR2] = acc.eth_addr;
        encoded_tx[tx_detail_idx::SIGN2] = acc.sign;
        encoded_tx[tx_detail_idx::AY2] = acc.ay;

        let mut raw_tx = RawTx {
            tx_type: TxType::DepositToOld,
            payload: encoded_tx.to_vec(),
            balance_path0: proof.balance_path.clone(),
            balance_path1: proof.balance_path.clone(),
            balance_path2: proof.balance_path.clone(),
            balance_path3: proof.balance_path,
            order_path0: self.trivial_order_path_elements(),
            order_path1: self.trivial_order_path_elements(),
            order_root0: acc.order_root,
            order_root1: acc.order_root,
            account_path0: proof.account_path.clone(),
            account_path1: proof.account_path,
            root_before: proof.root,
            root_after: Fr::zero(),
        };

        let mut balance = old_balance;
        balance.add_assign(&tx.amount);
        self.set_token_balance(tx.account_id, tx.token_id, balance);

        raw_tx.root_after = self.root();
        self.add_raw_tx(raw_tx);
    }
    /*
    fillTransferTx(tx: TranferTx) {
      let fullTx = {
        from: tx.from,
        to: tx.to,
        token_id: tx.token_id,
        amount: tx.amount,
        fromNonce: self.accounts.get(tx.from).nonce,
        toNonce: self.accounts.get(tx.to).nonce,
        old_balanceFrom: self.get_token_balance(tx.from, tx.token_id),
        old_balanceTo: self.get_token_balance(tx.to, tx.token_id),
      };
      return fullTx;
    }
    fillWithdraw_tx(tx: Withdraw_tx) {
      let fullTx = {
        account_id: tx.account_id,
        token_id: tx.token_id,
        amount: tx.amount,
        nonce: self.accounts.get(tx.account_id).nonce,
        old_balance: self.get_token_balance(tx.account_id, tx.token_id),
      };
      return fullTx;
    }
    Transfer(tx: TranferTx) {
      assert!(self.accounts.get(tx.from).eth_addr != 0n, "TransferTx: empty fromAccount");
      assert!(self.accounts.get(tx.to).eth_addr != 0n, "Transfer: empty toAccount");
      let proofFrom = self.state_proof(tx.from, tx.token_id);
      let fromAccount = self.accounts.get(tx.from);
      let toAccount = self.accounts.get(tx.to);

      // first, generate the tx
      let encoded_tx: Array<Fr> = new Array(Txlen());
      encoded_tx.fill(0n, 0, Txlen());

      let fromOldBalance = self.get_token_balance(tx.from, tx.token_id);
      let toOldBalance = self.get_token_balance(tx.to, tx.token_id);
      assert!(fromOldBalance > tx.amount, "Transfer balance not enough");
      encoded_tx[tx_detail_idx::ACCOUNT_ID1] = tx.from;
      encoded_tx[tx_detail_idx::ACCOUNT_ID2] = tx.to;
      encoded_tx[tx_detail_idx::TOKEN_ID] = tx.token_id;
      encoded_tx[tx_detail_idx::AMOUNT] = tx.amount;
      encoded_tx[tx_detail_idx::NONCE1] = fromAccount.nonce;
      encoded_tx[tx_detail_idx::NONCE2] = toAccount.nonce;
      encoded_tx[tx_detail_idx::SIGN1] = fromAccount.sign;
      encoded_tx[tx_detail_idx::SIGN2] = toAccount.sign;
      encoded_tx[tx_detail_idx::AY1] = fromAccount.ay;
      encoded_tx[tx_detail_idx::AY2] = toAccount.ay;
      encoded_tx[tx_detail_idx::ETH_ADDR1] = fromAccount.eth_addr;
      encoded_tx[tx_detail_idx::ETH_ADDR2] = toAccount.eth_addr;
      encoded_tx[tx_detail_idx::BALANCE1] = fromOldBalance;
      encoded_tx[tx_detail_idx::BALANCE2] = toOldBalance;
      encoded_tx[tx_detail_idx::SIG_L2_HASH] = tx.signature.hash;
      encoded_tx[tx_detail_idx::S] = tx.signature.S;
      encoded_tx[tx_detail_idx::R8X] = tx.signature.R8x;
      encoded_tx[tx_detail_idx::R8Y] = tx.signature.R8y;

      let raw_tx: RawTx = {
        tx_type: TxType.Transfer,
        payload: encoded_tx,
        balance_path0: proofFrom.balance_path,
        balance_path1: null,
        balance_path2: proofFrom.balance_path,
        balance_path3: null,
        order_path0: self.trivial_order_path_elements(),
        order_path1: self.trivial_order_path_elements(),
        order_root0: fromAccount.order_root,
        order_root1: toAccount.order_root,
        account_path0: proofFrom.account_path,
        account_path1: null,
        root_before: proofFrom.root,
        root_after: 0n,
      };

      self.set_token_balance(tx.from, tx.token_id, fromOldBalance - tx.amount);
      self.increase_nonce(tx.from);

      let proofTo = self.state_proof(tx.to, tx.token_id);
      raw_tx.balance_path1 = proofTo.balance_path;
      raw_tx.balance_path3 = proofTo.balance_path;
      raw_tx.account_path1 = proofTo.account_path;
      self.set_token_balance(tx.to, tx.token_id, toOldBalance + tx.amount);

      raw_tx.root_after = self.root();
      self.add_raw_tx(raw_tx);
    }
    Withdraw(tx: Withdraw_tx) {
      assert!(self.accounts.get(tx.account_id).eth_addr != 0n, "Withdraw");
      let proof = self.state_proof(tx.account_id, tx.token_id);
      // first, generate the tx
      let encoded_tx: Array<Fr> = new Array(Txlen());
      encoded_tx.fill(0n, 0, Txlen());

      let acc = self.accounts.get(tx.account_id);
      let balanceBefore = self.get_token_balance(tx.account_id, tx.token_id);
      assert!(balanceBefore > tx.amount, "Withdraw balance");
      encoded_tx[tx_detail_idx::ACCOUNT_ID1] = tx.account_id;
      encoded_tx[tx_detail_idx::TOKEN_ID] = tx.token_id;
      encoded_tx[tx_detail_idx::AMOUNT] = tx.amount;
      encoded_tx[tx_detail_idx::NONCE1] = acc.nonce;
      encoded_tx[tx_detail_idx::SIGN1] = acc.sign;
      encoded_tx[tx_detail_idx::AY1] = acc.ay;
      encoded_tx[tx_detail_idx::ETH_ADDR1] = acc.eth_addr;
      encoded_tx[tx_detail_idx::BALANCE1] = balanceBefore;

      encoded_tx[tx_detail_idx::SIG_L2_HASH] = tx.signature.hash;
      encoded_tx[tx_detail_idx::S] = tx.signature.S;
      encoded_tx[tx_detail_idx::R8X] = tx.signature.R8x;
      encoded_tx[tx_detail_idx::R8Y] = tx.signature.R8y;

      let raw_tx: RawTx = {
        tx_type: TxType.Withdraw,
        payload: encoded_tx,
        balance_path0: proof.balance_path,
        balance_path1: proof.balance_path,
        balance_path2: proof.balance_path,
        balance_path3: proof.balance_path,
        order_path0: self.trivial_order_path_elements(),
        order_path1: self.trivial_order_path_elements(),
        order_root0: acc.order_root,
        order_root1: acc.order_root,
        account_path0: proof.account_path,
        account_path1: proof.account_path,
        root_before: proof.root,
        root_after: 0n,
      };

      self.set_token_balance(tx.account_id, tx.token_id, balanceBefore - tx.amount);
      self.increase_nonce(tx.account_id);

      raw_tx.root_after = self.root();
      self.add_raw_tx(raw_tx);
    }
    */
    pub fn place_order(&mut self, tx: PlaceOrderTx) -> u32 {
        if self.verbose {
            println!("PlaceOrder {:#?} operation id {}", tx, self.buffered_txs.len());
        }
        // TODO: check order signature
        //assert!(self.accounts.get(tx.account_id).eth_addr != 0n, "PlaceOrder account: account_id" + tx.account_id);

        let account = *self.accounts.get(&tx.account_id).unwrap();
        let proof = self.state_proof(tx.account_id, tx.token_id_sell);

        let mut raw_tx = RawTx {
            tx_type: TxType::PlaceOrder,
            payload: Default::default(),
            balance_path0: proof.balance_path.clone(),
            balance_path1: proof.balance_path.clone(),
            balance_path2: proof.balance_path.clone(),
            balance_path3: proof.balance_path,
            order_path0: Default::default(),
            order_path1: self.trivial_order_path_elements(),
            order_root0: account.order_root,
            order_root1: Default::default(),
            account_path0: proof.account_path.clone(),
            account_path1: proof.account_path,
            root_before: self.root(),
            root_after: Default::default(),
        };
        //println!("orderRoo0", raw_tx.order_root0);

        let (order_pos, old_order) = self.create_new_order(&tx);

        // fill in the tx
        let mut tx_data: place_order_tx_data::PlaceOrderTxData = Default::default();

        tx_data.order_pos = u32_to_fr(order_pos);
        tx_data.old_order_id = old_order.order_id;
        tx_data.new_order_id = u32_to_fr(tx.order_id);
        tx_data.old_order_tokensell = old_order.tokensell;
        tx_data.old_order_filledsell = old_order.filled_sell;
        tx_data.old_order_amountsell = old_order.total_sell;
        tx_data.old_order_tokenbuy = old_order.tokenbuy;
        tx_data.old_order_filledbuy = old_order.filled_buy;
        tx_data.old_order_amountbuy = old_order.total_buy;
        tx_data.new_order_tokensell = u32_to_fr(tx.token_id_sell);
        tx_data.new_order_amountsell = tx.amount_sell;
        tx_data.new_order_tokenbuy = u32_to_fr(tx.token_id_buy);
        tx_data.new_order_amountbuy = tx.amount_buy;
        tx_data.accountID = u32_to_fr(tx.account_id);
        tx_data.balance = proof.leaf;
        tx_data.nonce = account.nonce;
        tx_data.sign = account.sign;
        tx_data.ay = account.ay;
        tx_data.ethAddr = account.eth_addr;

        raw_tx.payload = tx_data.encode();
        raw_tx.order_path0 = self.order_trees.get(&tx.account_id).unwrap().get_proof(order_pos).path_elements;
        //println!("raw_tx.order_path0", raw_tx.order_path0)
        raw_tx.order_root1 = self.order_trees.get(&tx.account_id).unwrap().get_proof(order_pos).root;

        raw_tx.root_after = self.root();
        self.add_raw_tx(raw_tx);
        if self.verbose {
            //println!("create order ", order_id, tx);
        }
        order_pos
    }
    pub fn spot_trade(&mut self, tx: SpotTradeTx) {
        //assert!(self.accounts.get(tx.order1_account_id).eth_addr != 0n, "SpotTrade account1");
        //assert!(self.accounts.get(tx.order2_account_id).eth_addr != 0n, "SpotTrade account2");

        assert!(tx.order1_id < 2u32.pow(self.order_levels as u32), "order1 id overflows");
        assert!(tx.order2_id < 2u32.pow(self.order_levels as u32), "order2 id overflows");

        let account1 = self.accounts.get(&tx.order1_account_id).unwrap();
        let account2 = self.accounts.get(&tx.order2_account_id).unwrap();
        let proof_order1_seller = self.state_proof(tx.order1_account_id, tx.token_id_1to2);
        let proof_order2_seller = self.state_proof(tx.order2_account_id, tx.token_id_2to1);

        let order1_pos = self.get_order_pos_by_id(tx.order1_account_id, tx.order1_id);
        let order2_pos = self.get_order_pos_by_id(tx.order2_account_id, tx.order2_id);

        let order1 = *self.order_map.get(&tx.order1_account_id).unwrap().get(&order1_pos).unwrap();
        let order2 = *self.order_map.get(&tx.order2_account_id).unwrap().get(&order2_pos).unwrap();

        // first, generate the tx

        let mut encoded_tx = [Fr::zero(); TX_LENGTH];
        encoded_tx[tx_detail_idx::ACCOUNT_ID1] = u32_to_fr(tx.order1_account_id);
        encoded_tx[tx_detail_idx::ACCOUNT_ID2] = u32_to_fr(tx.order2_account_id);
        encoded_tx[tx_detail_idx::ETH_ADDR1] = account1.eth_addr;
        encoded_tx[tx_detail_idx::ETH_ADDR2] = account2.eth_addr;
        encoded_tx[tx_detail_idx::SIGN1] = account1.sign;
        encoded_tx[tx_detail_idx::SIGN2] = account2.sign;
        encoded_tx[tx_detail_idx::AY1] = account1.ay;
        encoded_tx[tx_detail_idx::AY2] = account2.ay;
        encoded_tx[tx_detail_idx::NONCE1] = account1.nonce;
        encoded_tx[tx_detail_idx::NONCE2] = account2.nonce;
        let account1_balance_sell = self.get_token_balance(tx.order1_account_id, tx.token_id_1to2);
        let account2_balance_buy = self.get_token_balance(tx.order2_account_id, tx.token_id_1to2);
        let account2_balance_sell = self.get_token_balance(tx.order2_account_id, tx.token_id_2to1);
        let account1_balance_buy = self.get_token_balance(tx.order1_account_id, tx.token_id_2to1);
        assert!(account1_balance_sell > tx.amount_1to2, "balance_1to2");
        assert!(account2_balance_sell > tx.amount_2to1, "balance_2to1");
        encoded_tx[tx_detail_idx::TOKEN_ID] = u32_to_fr(tx.token_id_1to2);
        encoded_tx[tx_detail_idx::AMOUNT] = tx.amount_1to2;
        encoded_tx[tx_detail_idx::BALANCE1] = account1_balance_sell;
        encoded_tx[tx_detail_idx::BALANCE2] = account2_balance_buy;
        encoded_tx[tx_detail_idx::BALANCE3] = account2_balance_sell;
        encoded_tx[tx_detail_idx::BALANCE4] = account1_balance_buy;
        encoded_tx[tx_detail_idx::TOKEN_ID2] = u32_to_fr(tx.token_id_2to1);
        encoded_tx[tx_detail_idx::AMOUNT2] = tx.amount_2to1;
        encoded_tx[tx_detail_idx::TOKEN_ID3] = u32_to_fr(order1_pos);
        encoded_tx[tx_detail_idx::ORDER1_ID] = u32_to_fr(tx.order1_id);
        encoded_tx[tx_detail_idx::ORDER1_AMOUNT_SELL] = order1.total_sell;
        encoded_tx[tx_detail_idx::ORDER1_AMOUNT_BUY] = order1.total_buy;
        encoded_tx[tx_detail_idx::ORDER1_FILLED_SELL] = order1.filled_sell;
        encoded_tx[tx_detail_idx::ORDER1_FILLED_BUY] = order1.filled_buy;
        encoded_tx[tx_detail_idx::TOKEN_ID4] = u32_to_fr(order2_pos);
        encoded_tx[tx_detail_idx::ORDER2_ID] = u32_to_fr(tx.order2_id);
        encoded_tx[tx_detail_idx::ORDER2_AMOUNT_SELL] = order2.total_sell;
        encoded_tx[tx_detail_idx::ORDER2_AMOUNT_BUY] = order2.total_buy;
        encoded_tx[tx_detail_idx::ORDER2_FILLED_SELL] = order2.filled_sell;
        encoded_tx[tx_detail_idx::ORDER2_FILLED_BUY] = order2.filled_buy;

        let mut raw_tx = RawTx {
            tx_type: TxType::SpotTrade,
            payload: encoded_tx.to_vec(),
            balance_path0: proof_order1_seller.balance_path,
            balance_path1: Default::default(),
            balance_path2: proof_order2_seller.balance_path,
            balance_path3: Default::default(),
            order_path0: self
                .order_trees
                .get(&tx.order1_account_id)
                .unwrap()
                .get_proof(order1_pos)
                .path_elements,
            order_path1: self
                .order_trees
                .get(&tx.order2_account_id)
                .unwrap()
                .get_proof(order2_pos)
                .path_elements,
            order_root0: account1.order_root, // not really used in the circuit
            order_root1: account2.order_root, // not really used in the circuit
            account_path0: proof_order1_seller.account_path,
            account_path1: Default::default(),
            root_before: self.root(),
            root_after: Default::default(),
        };

        // do not update state root
        // account1 after sending, before receiving
        let mut balance1 = account1_balance_sell;
        balance1.sub_assign(&tx.amount_1to2);
        self.balance_trees
            .get_mut(&tx.order1_account_id)
            .unwrap()
            .set_value(tx.token_id_1to2, balance1);
        raw_tx.balance_path3 = self
            .balance_trees
            .get(&tx.order1_account_id)
            .unwrap()
            .get_proof(tx.token_id_2to1)
            .path_elements;
        // account2 after sending, before receiving
        let mut balance2 = account2_balance_sell;
        balance2.sub_assign(&tx.amount_2to1);
        self.balance_trees
            .get_mut(&tx.order2_account_id)
            .unwrap()
            .set_value(tx.token_id_2to1, balance2);
        raw_tx.balance_path1 = self
            .balance_trees
            .get(&tx.order2_account_id)
            .unwrap()
            .get_proof(tx.token_id_1to2)
            .path_elements;

        let mut order1_filled_sell = order1.filled_sell;
        order1_filled_sell.add_assign(&tx.amount_1to2);
        let mut order1_filled_buy = order1.filled_buy;
        order1_filled_buy.add_assign(&tx.amount_2to1);
        let new_order1 = Order {
            order_id: order1.order_id,
            tokenbuy: u32_to_fr(tx.token_id_2to1),
            tokensell: u32_to_fr(tx.token_id_1to2),
            filled_sell: order1_filled_sell,
            filled_buy: order1_filled_buy,
            total_sell: order1.total_sell,
            total_buy: order1.total_buy,
        };
        self.set_account_order(tx.order1_account_id, order1_pos, new_order1);
        let mut account1_balance_buy = account1_balance_buy;
        account1_balance_buy.add_assign(&tx.amount_2to1);
        self.set_token_balance(tx.order1_account_id, tx.token_id_2to1, account1_balance_buy);
        raw_tx.account_path1 = self.account_tree.get_proof(tx.order2_account_id).path_elements;

        let mut order2_filled_sell = order2.filled_sell;
        order2_filled_sell.add_assign(&tx.amount_2to1);
        let mut order2_filled_buy = order2.filled_buy;
        order2_filled_buy.add_assign(&tx.amount_1to2);
        let new_order2 = Order {
            order_id: order2.order_id,
            tokenbuy: u32_to_fr(tx.token_id_1to2),
            tokensell: u32_to_fr(tx.token_id_2to1),
            filled_sell: order2_filled_sell,
            filled_buy: order2_filled_buy,
            total_sell: order2.total_sell,
            total_buy: order2.total_buy,
        };
        self.set_account_order(tx.order2_account_id, order2_pos, new_order2);
        let mut account2_balance_buy = account2_balance_buy;
        account2_balance_buy.add_assign(&tx.amount_1to2);
        self.set_token_balance(tx.order2_account_id, tx.token_id_1to2, account2_balance_buy);

        raw_tx.root_after = self.root();
        self.add_raw_tx(raw_tx);
    }
    pub fn nop(&mut self) {
        // assume we already have initialized the account tree and the balance tree
        let trivial_proof = self.state_proof(0, 0);
        let encoded_tx = [Fr::zero(); TX_LENGTH];
        let raw_tx = RawTx {
            tx_type: TxType::Nop,
            payload: encoded_tx.to_vec(),
            balance_path0: trivial_proof.balance_path.clone(),
            balance_path1: trivial_proof.balance_path.clone(),
            balance_path2: trivial_proof.balance_path.clone(),
            balance_path3: trivial_proof.balance_path,
            order_path0: self.trivial_order_path_elements(),
            order_path1: self.trivial_order_path_elements(),
            order_root0: trivial_proof.order_root,
            order_root1: trivial_proof.order_root,
            account_path0: trivial_proof.account_path.clone(),
            account_path1: trivial_proof.account_path,
            root_before: self.root(),
            root_after: self.root(),
        };
        self.add_raw_tx(raw_tx);
    }

    pub fn flush_with_nop(&mut self) {
        while self.buffered_txs.len() % self.n_tx != 0 {
            self.nop();
        }
    }
}
