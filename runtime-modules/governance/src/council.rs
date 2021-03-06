use rstd::prelude::*;
use sr_primitives::traits::{One, Zero};
use srml_support::{debug, decl_event, decl_module, decl_storage, ensure};
use system::{self, ensure_root};

pub use super::election::{self, CouncilElected, Seat, Seats};
pub use common::currency::{BalanceOf, GovernanceCurrency};

// Hook For announcing that council term has ended
pub trait CouncilTermEnded {
    fn council_term_ended();
}

impl CouncilTermEnded for () {
    fn council_term_ended() {}
}

impl<X: CouncilTermEnded> CouncilTermEnded for (X,) {
    fn council_term_ended() {
        X::council_term_ended();
    }
}

pub trait Trait: system::Trait + recurringrewards::Trait + GovernanceCurrency {
    type Event: From<Event<Self>> + Into<<Self as system::Trait>::Event>;

    type CouncilTermEnded: CouncilTermEnded;
}

decl_storage! {
    trait Store for Module<T: Trait> as Council {
        pub ActiveCouncil get(active_council) config(): Seats<T::AccountId, BalanceOf<T>>;

        pub TermEndsAt get(term_ends_at) config() : T::BlockNumber = T::BlockNumber::from(1);

        /// The mint that funds council member rewards and spending proposals budget. It is an Option
        /// because it was introduced in a runtime upgrade. It will be automatically created when
        /// a successful call to set_council_mint_capacity() is made.
        pub CouncilMint get(council_mint) : Option<<T as minting::Trait>::MintId>;

        /// The reward relationships currently in place. There may not necessarily be a 1-1 correspondance with
        /// the active council, since there are multiple ways of setting/adding/removing council members, some of which
        /// do not involve creating a relationship.
        pub RewardRelationships get(reward_relationships) : map T::AccountId => T::RewardRelationshipId;

        /// Reward amount paid out at each PayoutInterval
        pub AmountPerPayout get(amount_per_payout): minting::BalanceOf<T>;

        /// Optional interval in blocks on which a reward payout will be made to each council member
        pub PayoutInterval get(payout_interval): Option<T::BlockNumber>;

        /// How many blocks after the reward is created, the first payout will be made
        pub FirstPayoutAfterRewardCreated get(first_payout_after_reward_created): T::BlockNumber;
    }
}

// Event for this module.
decl_event!(
    pub enum Event<T> where <T as system::Trait>::BlockNumber {
        CouncilTermEnded(BlockNumber),
        NewCouncilTermStarted(BlockNumber),
    }
);

impl<T: Trait> CouncilElected<Seats<T::AccountId, BalanceOf<T>>, T::BlockNumber> for Module<T> {
    fn council_elected(seats: Seats<T::AccountId, BalanceOf<T>>, term: T::BlockNumber) {
        <ActiveCouncil<T>>::put(seats.clone());

        let next_term_ends_at = <system::Module<T>>::block_number() + term;

        <TermEndsAt<T>>::put(next_term_ends_at);

        if let Some(reward_source) = Self::council_mint() {
            for seat in seats.iter() {
                Self::add_reward_relationship(&seat.member, reward_source);
            }
        } else {
            // Skip trying to create rewards since no mint has been created yet
            debug::warn!(
                "Not creating reward relationship for council seats because no mint exists"
            );
        }

        Self::deposit_event(RawEvent::NewCouncilTermStarted(next_term_ends_at));
    }
}

impl<T: Trait> Module<T> {
    pub fn is_term_ended() -> bool {
        <system::Module<T>>::block_number() >= Self::term_ends_at()
    }

    pub fn is_councilor(sender: &T::AccountId) -> bool {
        Self::active_council().iter().any(|c| c.member == *sender)
    }

    /// Initializes a new mint, discarding previous mint if it existed.
    pub fn create_new_council_mint(
        capacity: minting::BalanceOf<T>,
    ) -> Result<T::MintId, &'static str> {
        let mint_id = <minting::Module<T>>::add_mint(capacity, None)?;
        CouncilMint::<T>::put(mint_id);
        Ok(mint_id)
    }

    fn add_reward_relationship(destination: &T::AccountId, reward_source: T::MintId) {
        let recipient = <recurringrewards::Module<T>>::add_recipient();

        // When calculating when first payout occurs, add minimum of one block interval to ensure rewards module
        // has a chance to execute its on_finalize routine.
        let next_payout_at = system::Module::<T>::block_number()
            + Self::first_payout_after_reward_created()
            + T::BlockNumber::one();

        if let Ok(relationship_id) = <recurringrewards::Module<T>>::add_reward_relationship(
            reward_source,
            recipient,
            destination.clone(),
            Self::amount_per_payout(),
            next_payout_at,
            Self::payout_interval(),
        ) {
            RewardRelationships::<T>::insert(destination, relationship_id);
        } else {
            debug::warn!("Failed to create a reward relationship for council seat");
        }
    }

    fn remove_reward_relationships() {
        for seat in Self::active_council().into_iter() {
            if RewardRelationships::<T>::exists(&seat.member) {
                let id = Self::reward_relationships(&seat.member);
                <recurringrewards::Module<T>>::remove_reward_relationship(id);
            }
        }
    }

    fn on_term_ended(now: T::BlockNumber) {
        // Stop paying out rewards when the term ends.
        // Note: Is it not simpler to just do a single payout at end of term?
        // During the term the recurring reward module could unfairly pay some but not all council members
        // If there is insufficient mint capacity.. so doing it at this point offers more control
        // and a potentially more fair outcome in such a case.
        Self::remove_reward_relationships();

        Self::deposit_event(RawEvent::CouncilTermEnded(now));

        T::CouncilTermEnded::council_term_ended();
    }
}

decl_module! {
    pub struct Module<T: Trait> for enum Call where origin: T::Origin {
        fn deposit_event() = default;

        fn on_finalize(now: T::BlockNumber) {
            if now == Self::term_ends_at() {
                Self::on_term_ended(now);
            }
        }

        // Privileged methods

        /// Force set a zero staked council. Stakes in existing council seats are not returned.
        /// Existing council rewards are removed and new council members do NOT get any rewards.
        /// Avoid using this call if possible, will be deprecated. The term of the new council is
        /// not extended.
        pub fn set_council(origin, accounts: Vec<T::AccountId>) {
            ensure_root(origin)?;

            // Council is being replaced so remove existing reward relationships if they exist
            Self::remove_reward_relationships();

            if let Some(reward_source) = Self::council_mint() {
                for account in accounts.clone() {
                    Self::add_reward_relationship(&account, reward_source);
                }
            }

            let new_council: Seats<T::AccountId, BalanceOf<T>> = accounts.into_iter().map(|account| {
                Seat {
                    member: account,
                    stake: BalanceOf::<T>::zero(),
                    backers: vec![]
                }
            }).collect();

            <ActiveCouncil<T>>::put(new_council);
        }

        /// Adds a zero staked council member. A member added in this way does not get a recurring reward.
        fn add_council_member(origin, account: T::AccountId) {
            ensure_root(origin)?;

            ensure!(!Self::is_councilor(&account), "cannot add same account multiple times");

            if let Some(reward_source) = Self::council_mint() {
                Self::add_reward_relationship(&account, reward_source);
            }

            let seat = Seat {
                member: account,
                stake: BalanceOf::<T>::zero(),
                backers: vec![]
            };

            // add member to existing council
            <ActiveCouncil<T>>::mutate(|council| council.push(seat));
        }

        /// Remove a single council member and their reward.
        fn remove_council_member(origin, account_to_remove: T::AccountId) {
            ensure_root(origin)?;

            ensure!(Self::is_councilor(&account_to_remove), "account is not a councilor");

            if RewardRelationships::<T>::exists(&account_to_remove) {
                let relationship_id = Self::reward_relationships(&account_to_remove);
                <recurringrewards::Module<T>>::remove_reward_relationship(relationship_id);
            }

            let filtered_council: Seats<T::AccountId, BalanceOf<T>> = Self::active_council()
                .into_iter()
                .filter(|c| c.member != account_to_remove)
                .collect();

            <ActiveCouncil<T>>::put(filtered_council);
        }

        /// Set blocknumber when council term will end
        fn set_term_ends_at(origin, ends_at: T::BlockNumber) {
            ensure_root(origin)?;
            ensure!(ends_at > <system::Module<T>>::block_number(), "must set future block number");
            <TermEndsAt<T>>::put(ends_at);
        }

        /// Sets the capacity of the the council mint, if it doesn't exist, attempts to
        /// create a new one.
        pub fn set_council_mint_capacity(origin, capacity: minting::BalanceOf<T>) {
            ensure_root(origin)?;

            if let Some(mint_id) = Self::council_mint() {
                minting::Module::<T>::set_mint_capacity(mint_id, capacity)?;
            } else {
                Self::create_new_council_mint(capacity)?;
            }
        }

        /// Attempts to mint and transfer amount to destination account
        fn spend_from_council_mint(origin, amount: minting::BalanceOf<T>, destination: T::AccountId) {
            ensure_root(origin)?;

            if let Some(mint_id) = Self::council_mint() {
                minting::Module::<T>::transfer_tokens(mint_id, amount, &destination)?;
            } else {
                return Err("CouncilHasNoMint")
            }
        }

        /// Sets the council rewards which is only applied on new council being elected.
        fn set_council_rewards(
            origin,
            amount_per_payout: minting::BalanceOf<T>,
            payout_interval: Option<T::BlockNumber>,
            first_payout_after_reward_created: T::BlockNumber
        ) {
            ensure_root(origin)?;

            AmountPerPayout::<T>::put(amount_per_payout);
            FirstPayoutAfterRewardCreated::<T>::put(first_payout_after_reward_created);

            if let Some(payout_interval) = payout_interval {
                PayoutInterval::<T>::put(payout_interval);
            } else {
                PayoutInterval::<T>::take();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::*;
    use srml_support::*;

    fn add_council_member_as_root(account: <Test as system::Trait>::AccountId) -> dispatch::Result {
        Council::add_council_member(system::RawOrigin::Root.into(), account)
    }

    #[test]
    fn add_council_member_test() {
        initial_test_ext().execute_with(|| {
            assert!(!Council::is_councilor(&1));

            assert_ok!(add_council_member_as_root(1));
            assert!(Council::is_councilor(&1));

            assert_ok!(add_council_member_as_root(2));
            assert!(Council::is_councilor(&1));
            assert!(Council::is_councilor(&2));
        });
    }

    #[test]
    fn remove_council_member_test() {
        initial_test_ext().execute_with(|| {
            assert_ok!(add_council_member_as_root(1));
            assert_ok!(add_council_member_as_root(2));
            assert_ok!(add_council_member_as_root(3));

            assert_ok!(Council::remove_council_member(
                system::RawOrigin::Root.into(),
                2
            ));

            assert!(!Council::is_councilor(&2));
            assert!(Council::is_councilor(&1));
            assert!(Council::is_councilor(&3));
        });
    }

    #[test]
    fn set_council_test() {
        initial_test_ext().execute_with(|| {
            assert_ok!(Council::set_council(
                system::RawOrigin::Root.into(),
                vec![4, 5, 6]
            ));
            assert!(Council::is_councilor(&4));
            assert!(Council::is_councilor(&5));
            assert!(Council::is_councilor(&6));
        });
    }

    #[test]
    fn council_elected_test() {
        initial_test_ext().execute_with(|| {
            // Ensure a mint is created so we can create rewards
            assert_ok!(Council::set_council_mint_capacity(
                system::RawOrigin::Root.into(),
                1000
            ));

            Council::council_elected(
                vec![
                    Seat {
                        member: 5,
                        stake: 0,
                        backers: vec![],
                    },
                    Seat {
                        member: 6,
                        stake: 0,
                        backers: vec![],
                    },
                    Seat {
                        member: 7,
                        stake: 0,
                        backers: vec![],
                    },
                ],
                50 as u64, // <Test as system::Trait>::BlockNumber::from(50)
            );

            assert!(Council::is_councilor(&5));
            assert!(Council::is_councilor(&6));
            assert!(Council::is_councilor(&7));

            assert!(RewardRelationships::<Test>::exists(&5));
            assert!(RewardRelationships::<Test>::exists(&6));
            assert!(RewardRelationships::<Test>::exists(&7));
        });
    }
}
