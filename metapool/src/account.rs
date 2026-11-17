use crate::*;
use near_sdk::log;

pub use crate::types::*;
pub use crate::utils::*;

// -----------------
// User Account Data
// -----------------
#[derive(BorshDeserialize, BorshSerialize, Debug, PartialEq)]
pub struct Account {
    /// This amount increments with deposits and decrements with for_staking
    /// increments with complete_unstake and decrements with user withdrawals from the contract
    /// withdrawals from the pools can include rewards
    /// since staking is delayed and in batches it only eventually matches env::balance()
    /// total = available + staked + unstaked
    /// Note: In the simplified user-UI, the basic-user always does deposit-and-stake and sell/unstake that goes directly to their wallet
    /// so the only users of this field are lockup-contracts and advanced-users when they perform "Classic Unstakes"
    pub available: u128,

    /// The amount of st_near (stake shares) of the total staked balance in the pool(s) this user owns.
    /// When someone stakes, share-price is computed and shares are "sold" to the user so he only owns what he's staking and no rewards yet
    /// When a user request a transfer to other user, shares from the origin are moved to shares of the destination
    /// The share_price can be computed as total_for_staking/total_stake_shares
    /// stNEAR price = total_for_staking/total_stake_shares
    pub stake_shares: u128, //st_near this account owns

    /// Incremented when the user asks for Delayed-Unstaking. The amount of unstaked near in the pools
    pub unstaked: u128,

    /// The epoch height when the unstaked will be available
    /// The funds will be locked for -AT LEAST- NUM_EPOCHS_TO_UNLOCK epochs
    pub unstaked_requested_unlock_epoch: EpochHeight,

    //-- META (now mpDAO) INCENTIVES (Disabled on 2023-05)
    // realized META, can be used to transfer META from one user to another
    // Total META = realized_meta + staking_meter.mul_rewards(valued_stake_shares) + lp_meter.mul_rewards(valued_lp_shares)
    // Every time the user operates on STAKE/UNSTAKE: we realize meta: realized_meta += staking_meter.mul_rewards(valued_staked_shares)
    // Every time the user operates on ADD.LIQ/REM.LIQ.: we realize meta: realized_meta += lp_meter.mul_rewards(valued_lp_shares)
    // if the user calls farm_meta() we perform both
    pub realized_meta: u128, // deprecated
    ///Staking rewards meter (to mint stNEAR for the user)
    pub staking_meter: RewardMeter,
    ///LP fee gains meter (to mint meta for the user)
    pub lp_meter: RewardMeter, // deprecated

    //-- STATISTICAL DATA --
    // User's statistical data
    // This is the user-controlled staking rewards meter, it works as a car's "trip meter". The user can reset them to zero.
    // to compute trip_rewards we start from current_stnear, undo unstakes, undo stakes and finally subtract trip_start_stnear
    // trip_rewards = current_stnear + trip_accum_unstakes - trip_accum_stakes - trip_start_stnear;
    /// trip_start: (timestamp in milliseconds) this field is set at account creation, so it will start metering rewards
    pub trip_start: Timestamp,

    /// OBSOLETE - How much stnear the user had at "trip_start".
    pub trip_start_stnear: u128,
    // how much stnear the staked since trip start (minus unstaked)
    pub trip_accum_stakes: u128,
    // how much the user unstaked since trip start (zeroed if there was stake)
    pub trip_accum_unstakes: u128,

    ///NS liquidity pool shares, if the user is a liquidity provider
    pub nslp_shares: u128,
}

/// User account on this contract
impl Default for Account {
    fn default() -> Self {
        Self {
            available: 0,
            stake_shares: 0,
            unstaked: 0,
            unstaked_requested_unlock_epoch: 0,
            //meta & reward-meters
            realized_meta: 0,
            staking_meter: RewardMeter::default(),
            lp_meter: RewardMeter::default(),
            //trip-meter fields
            trip_start: env::block_timestamp() / 1_000_000, //converted from nanoseconds to milliseconds
            trip_start_stnear: 0,
            trip_accum_stakes: 0,
            trip_accum_unstakes: 0,
            //NS liquidity pool
            nslp_shares: 0,
        }
    }
}
impl Account {

    /// if the account can be closed and storage deposit returned
    pub fn can_be_closed(&self) -> bool {
        self.available == 0
        && self.unstaked == 0
        && self.stake_shares == 0
        && self.nslp_shares == 0
    }

    #[inline]
    pub fn valued_nslp_shares(&self, main: &MetaPool, nslp_account: &Account) -> u128 {
        main.amount_from_nslp_shares(self.nslp_shares, &nslp_account)
    }

    //----------------
    // add st_nears, considering it as "a stake" for trip-meter purposes
    pub fn add_st_near(&mut self, st_near_amount: u128, main: &MetaPool) {
        self.add_stake_shares(
            st_near_amount,
            main.amount_from_stake_shares(st_near_amount),
        )
    }
    pub fn add_stake_shares(&mut self, num_shares: u128, near_amount: u128) {
        self.stake_shares += num_shares;

        // to buy stnear is to stake
        self.trip_accum_stakes += near_amount;

        self.staking_meter.stake(near_amount);
    }

    // remove st_near considering is "an unstake" for trip-meter purposes
    pub fn sub_st_near(&mut self, st_near_amount: u128, main: &MetaPool) {
        self.sub_stake_shares(
            st_near_amount,
            main.amount_from_stake_shares(st_near_amount),
        )
    }
    pub fn sub_stake_shares(&mut self, num_shares: u128, near_amount: u128) {
        assert!(
            self.stake_shares >= num_shares,
            "sub_stake_shares self.stake_shares {} < num_shares {}",
            self.stake_shares,
            num_shares
        );
        self.stake_shares -= num_shares;

        // to sell stnear is to unstake
        self.trip_accum_unstakes += near_amount;
        if self.trip_accum_unstakes < self.trip_accum_stakes {
            //keep just the delta
            self.trip_accum_stakes -= self.trip_accum_unstakes;
            self.trip_accum_unstakes = 0;
        }

        self.staking_meter.unstake(near_amount);
    }

    /// user method
    /// completes unstake action by moving from acc.unstaked & main.retrieved_for_unstaked_claims -> acc.available & main.total_available
    pub fn in_memory_try_finish_unstaking(
        &mut self,
        account_id: &str,
        amount: u128,
        main: &mut MetaPool,
    ) -> u128 {
        assert!(
            amount <= self.unstaked,
            "Not enough unstaked balance {}",
            self.unstaked
        );

        let epoch = env::epoch_height();
        assert!( epoch >= self.unstaked_requested_unlock_epoch,
            "The unstaked balance is not yet available due to unstaking delay. You need to wait at least {} epochs"
            , self.unstaked_requested_unlock_epoch - epoch);

        // in the account, moves from unstaked to available
        self.unstaked -= amount; //Zeroes, claimed
        self.available += amount;
        //check the heart beat has really moved the funds
        assert!(
            main.retrieved_for_unstake_claims >= amount,
            "Funds are not yet available due to unstaking delay. Epoch:{}",
            env::epoch_height()
        );
        // in the contract, move from reserve_for_unstaked_claims to total_available
        main.retrieved_for_unstake_claims -= amount;
        assert!(main.total_unstake_claims >= amount, "ITUC");
        main.total_unstake_claims -= amount;
        main.total_available += amount;

        event!(
            r#"{{"event":"D-WITHD","account_id":"{}","amount":"{}"}}"#,
            account_id,
            amount
        );

        log!("{} unstaked moved to available", amount);

        return amount;
    }

    pub(crate) fn take_from_available(
        &mut self,
        account_id: &String,
        amount_requested: u128,
        main: &mut MetaPool,
    ) -> u128 {
        let to_withdraw:u128 =
        // if the amount is close to user's total, remove user's total
        // to: a) do not leave less than ONE_MILLI_NEAR in the account, b) Allow some yoctos of rounding, e.g. remove(100) removes 99.999993 without panicking
        // Audit Note: Do not do this for .lockup accounts because the lockup contract relies on precise amounts
        if is_lockup_account(account_id) || !is_close(amount_requested, self.available) { 
            // exact amount
            amount_requested
        }
        else {
            // is close, allow for rounding simplification
            self.available
        };

        assert!(
            self.available >= to_withdraw,
            "Not enough available balance {} for the requested amount",
            self.available
        );
        self.available -= to_withdraw;

        assert!(main.total_available >= to_withdraw, "i_s_Inconsistency");
        main.total_available -= to_withdraw;

        return to_withdraw;
    }
}

#[cfg(test)]
mod tests_unlock_epoch {
    use super::*;
    use near_sdk::test_utils::{accounts, VMContextBuilder};
    use near_sdk::{testing_env, MockedBlockchain};

    fn setup() -> MetaPool {
        testing_env!(VMContextBuilder::new()
            .current_account_id(accounts(0))
            .predecessor_account_id(accounts(0))
            .epoch_height(100)
            .build());
        MetaPool::new(
            accounts(0).to_string(),
            accounts(1).to_string(),
            accounts(2).to_string(),
            "meta.near".to_string(),
        )
    }

    fn setup_with_pool_and_user(pool_staked: u128, user_shares: u128) -> (MetaPool, String) {
        let mut c = setup();
        let mut sp = StakingPoolInfo::new("pool1.pool.near".to_string(), 10000);
        sp.staked = pool_staked;
        c.staking_pools.push(sp);
        let total = std::cmp::max(pool_staked, user_shares);
        c.total_for_staking = total;
        c.total_actually_staked = pool_staked;
        c.total_stake_shares = total;
        let user = accounts(3).to_string();
        let mut acc = Account::default();
        acc.stake_shares = user_shares;
        c.internal_update_account(&user, &acc);
        (c, user)
    }

    #[test]
    fn follow_up_unstake_does_not_move_unlock_epoch_earlier() {
        let (mut c, user) = setup_with_pool_and_user(1000 * ONE_NEAR, 200 * ONE_NEAR);
        let mut acc = c.internal_get_account(&user);
        acc.unstaked_requested_unlock_epoch = 118; // an earlier 8-epoch unstake
        c.internal_unstake_shares(&user, &mut acc, ONE_NEAR);
        assert_eq!(c.internal_get_account(&user).unstaked_requested_unlock_epoch, 118);
    }

    #[test]
    fn split_unstake_gets_the_same_delay_as_a_single_one() {
        // pool has 120 NEAR staked: one 100 NEAR unstake is a 4 epoch wait, but 100 + 30 exceeds the pool
        let (mut c, user) = setup_with_pool_and_user(120 * ONE_NEAR, 200 * ONE_NEAR);
        let mut acc = c.internal_get_account(&user);
        c.internal_unstake_shares(&user, &mut acc, 100 * ONE_NEAR);
        assert_eq!(acc.unstaked_requested_unlock_epoch, 100 + NUM_EPOCHS_TO_UNLOCK);
        c.internal_unstake_shares(&user, &mut acc, 30 * ONE_NEAR);
        assert_eq!(acc.unstaked_requested_unlock_epoch, 100 + 2 * NUM_EPOCHS_TO_UNLOCK);
    }
}
