use anchor_lang::prelude::*;
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token_interface::{self, Mint, TokenAccount, TokenInterface, TransferChecked};
use pyth_solana_receiver_sdk::price_update::{self, get_feed_id_from_hex, PriceUpdateV2};
use crate::state::*; // Assumes your Bank, User, etc., structs are here
use crate::error::ErrorCode; // Assumes your custom errors are here
// Define your mint addresses as constants for security and clarity
use crate::constants::{
    SOL_USD_FEED_ID, 
    USDC_USD_FEED_ID, 
    SOL_MINT_ADDRESS, 
    USDC_MINT_ADDRESS
};


//================================================================
// Accounts Struct for the Withdraw Instruction
//================================================================
#[derive(Accounts)]
pub struct Withdraw<'info> {
    /// The user initiating the withdrawal. They must sign the transaction.
    #[account(mut)]
    pub signer: Signer<'info>,

    /// The mint of the asset the user wants TO WITHDRAW.
    #[account(mut)]
    pub mint_to_withdraw: InterfaceAccount<'info, Mint>,

    /// The bank's state account for the asset being withdrawn. Required to calculate
    /// the correct token amount from the user's shares.
    #[account(
        mut, 
        seeds = [mint_to_withdraw.key().as_ref()], 
        bump
    )]
    pub bank: Account<'info, Bank>,

    /// The bank's vault (PDA) from which the user's tokens will be paid out.
    #[account(
        mut,
        seeds = [b"treasury", mint_to_withdraw.key().as_ref()],
        bump
    )]
    pub bank_token_account: InterfaceAccount<'info, TokenAccount>,

    /// The user's master account (PDA) which holds all their deposit and borrow info.
    /// This is the source of truth for the health check.
    #[account(
        mut, 
        seeds = [signer.key().as_ref()], 
        bump
    )]
    pub user_account: Account<'info, User>,

    /// The user's token account (ATA) where the withdrawn tokens will be sent.
    /// Anchor will create it if it doesn't exist, with the user paying the rent.
    #[account(
        init_if_needed,
        payer = signer,
        associated_token::mint = mint_to_withdraw,
        associated_token::authority = signer,
    )]
    pub user_token_account: InterfaceAccount<'info, TokenAccount>,

    /// The Pyth price feed account. This is ESSENTIAL to value all assets
    /// in the user's portfolio for the health check.
    pub price_update: Account<'info, PriceUpdateV2>,
    
    // Standard required programs
    pub token_program: Interface<'info, TokenInterface>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

//================================================================
// Instruction Logic for Processing a Withdrawal
//================================================================
pub fn process_withdraw(ctx: Context<Withdraw>, shares_to_withdraw: u64) -> Result<()> {
    // --- 1. Initial Sanity and Ownership Checks ---
    if shares_to_withdraw == 0 {
        return err!(ErrorCode::ZeroAmount);
    }

    let user = &ctx.accounts.user_account;
    let bank = &ctx.accounts.bank;

    // Determine which of the user's deposits we are targeting based on the mint.
    let (user_deposited_shares, user_deposited_amount) = 
        match ctx.accounts.mint_to_withdraw.key() {
            key if key == USDC_MINT_ADDRESS.parse().unwrap() => 
                (user.deposited_usdc_shares, user.deposited_usdc),
            key if key == SOL_MINT_ADDRESS.parse().unwrap() => 
                (user.deposited_sol_shares, user.deposited_sol),
            _ => return err!(ErrorCode::UnsupportedAsset),
        };

    // Check if the user actually owns enough shares to withdraw.
    if shares_to_withdraw > user_deposited_shares {
        msg!("Attempted to withdraw {} shares, but user only has {}", shares_to_withdraw, user_deposited_shares);
        return err!(ErrorCode::InsufficientShares);
    }
    
    // --- 2. Calculate Token Amount to Withdraw ---
    // The user specifies shares, and the protocol calculates the token amount.
    // This is safer than the reverse as it prevents rounding exploits against the protocol.
    // Formula: amount = (shares_to_withdraw * total_tokens_in_bank) / total_shares_in_bank
    let amount_to_withdraw = (shares_to_withdraw as u128)
        .checked_mul(bank.total_deposits as u128).ok_or(ErrorCode::MathOverflow)?
        .checked_div(bank.total_deposit_shares as u128).ok_or(ErrorCode::MathOverflow)? as u64;

    // Another sanity check. The calculated amount should not exceed what the user's account says they have.
    if amount_to_withdraw > user_deposited_amount {
        return err!(ErrorCode::InsufficientFunds);
    }

    // --- 3. THE CRITICAL HEALTH CHECK ---
    // This is the most important security check. We must simulate the withdrawal
    // and verify that the user's remaining collateral is sufficient to cover their
    // outstanding debt. We must prevent a user from withdrawing collateral that
    // would leave their position undercollateralized.
    msg!("Performing health check before allowing withdrawal...");
    
    // A. Get current prices for ALL assets in the user's portfolio (both collateral and debt).
    let clock = Clock::get()?;
    let price_update = &ctx.accounts.price_update;
    let sol_price = price_update.get_price_no_older_than(&clock, 60, &get_feed_id_from_hex(SOL_USD_FEED_ID)?)?;
    let usdc_price = price_update.get_price_no_older_than(&clock, 60, &get_feed_id_from_hex(USDC_USD_FEED_ID)?)?;

    // B. Calculate the total USD value of all of the user's DEBTS.
    let total_debt_value = (sol_price.price as u128 * user.borrowed_sol as u128)
        .checked_add(usdc_price.price as u128 * user.borrowed_usdc as u128)
        .ok_or(ErrorCode::MathOverflow)?;

    // C. If the user has debt, we must perform the health check.
    if total_debt_value > 0 {
        // D. SIMULATE the new collateral state *after* the withdrawal.
        let (simulated_sol_collateral, simulated_usdc_collateral) = match ctx.accounts.mint_to_withdraw.key() {
            key if key == USDC_MINT_ADDRESS.parse().unwrap() => 
                (user.deposited_sol, user_deposited_amount - amount_to_withdraw),
            key if key == SOL_MINT_ADDRESS.parse().unwrap() => 
                (user_deposited_amount - amount_to_withdraw, user.deposited_usdc),
            _ => return err!(ErrorCode::UnsupportedAsset), // Should be unreachable
        };

        // E. Calculate the total USD value of the user's collateral AFTER the withdrawal.
        let simulated_total_collateral_value = (sol_price.price as u128 * simulated_sol_collateral as u128)
            .checked_add(usdc_price.price as u128 * simulated_usdc_collateral as u128)
            .ok_or(ErrorCode::MathOverflow)?;
        
        // F. Apply the liquidation threshold to the simulated collateral value.
        // This tells us the maximum debt value this collateral can support before being liquidatable.
        // We assume a single liquidation_threshold for simplicity. A real protocol might have per-asset thresholds.
        let simulated_weighted_collateral = simulated_total_collateral_value
            .checked_mul(bank.liquidation_threshold as u128).ok_or(ErrorCode::MathOverflow)?
            .checked_div(100).ok_or(ErrorCode::MathOverflow)?; // For percentage
        
        // G. THE FINAL VERDICT: Is the remaining collateral value sufficient to cover the debt?
        // If this check fails, the transaction is reverted, protecting the protocol.
        if simulated_weighted_collateral < total_debt_value {
            msg!("Withdrawal rejected: would leave position unhealthy and open to liquidation.");
            msg!("Simulated Collateral Value: {}, Debt Value: {}", simulated_weighted_collateral, total_debt_value);
            return err!(ErrorCode::PositionUnhealthy);
        }
    }
    
    // --- 4. Execute Token Transfer (CPI) ---
    // This code only runs if the health check above has passed.
    msg!("Health check passed. Proceeding with transfer.");
    let signer_seeds: &[&[&[u8]]] = &[&[
        b"treasury", 
        ctx.accounts.mint_to_withdraw.to_account_info().key.as_ref(), 
        &[ctx.bumps.bank_token_account]
    ]];
    
    let cpi_accounts = TransferChecked {
        from: ctx.accounts.bank_token_account.to_account_info(),
        mint: ctx.accounts.mint_to_withdraw.to_account_info(),
        to: ctx.accounts.user_token_account.to_account_info(),
        authority: ctx.accounts.bank_token_account.to_account_info(), // The PDA is the authority
    };
    
    token_interface::transfer_checked(
        CpiContext::new(ctx.accounts.token_program.to_account_info(), cpi_accounts)
            .with_signer(signer_seeds), 
        amount_to_withdraw, 
        ctx.accounts.mint_to_withdraw.decimals
    )?;

    // --- 5. Update State (Correct Accounting) ---
    // If the transfer succeeds, we update our records to reflect the withdrawal.
    let bank_mut = &mut ctx.accounts.bank;
    let user_mut = &mut ctx.accounts.user_account;
    
    bank_mut.total_deposits = bank_mut.total_deposits.checked_sub(amount_to_withdraw).ok_or(ErrorCode::MathOverflow)?;
    bank_mut.total_deposit_shares = bank_mut.total_deposit_shares.checked_sub(shares_to_withdraw).ok_or(ErrorCode::MathOverflow)?;
    
    match ctx.accounts.mint_to_withdraw.key() {
        key if key == USDC_MINT_ADDRESS.parse().unwrap() => {
            user_mut.deposited_usdc = user_mut.deposited_usdc.checked_sub(amount_to_withdraw).ok_or(ErrorCode::MathOverflow)?;
            user_mut.deposited_usdc_shares = user_mut.deposited_usdc_shares.checked_sub(shares_to_withdraw).ok_or(ErrorCode::MathOverflow)?;
        }
        key if key == SOL_MINT_ADDRESS.parse().unwrap() => {
            user_mut.deposited_sol = user_mut.deposited_sol.checked_sub(amount_to_withdraw).ok_or(ErrorCode::MathOverflow)?;
            user_mut.deposited_sol_shares = user_mut.deposited_sol_shares.checked_sub(shares_to_withdraw).ok_or(ErrorCode::MathOverflow)?;
        }
        _ => return err!(ErrorCode::UnsupportedAsset), // Should be unreachable
    }

    msg!("Withdrawal successful. Amount: {}, Shares redeemed: {}", amount_to_withdraw, shares_to_withdraw);
    Ok(())
}

