use anchor_lang::prelude::*;
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token_interface::{self, Mint, TokenAccount, TokenInterface, TransferChecked};
use pyth_solana_receiver_sdk::price_update::{self, get_feed_id_from_hex, PriceUpdateV2};
use crate::state::*;
use crate::error::ErrorCode;
use crate::constants::{
    SOL_USD_FEED_ID, 
    USDC_USD_FEED_ID, 
    SOL_MINT_ADDRESS, 
    USDC_MINT_ADDRESS
};

//================================================================
// Accounts Struct for the Liquidate Instruction
//================================================================
#[derive(Accounts)]
pub struct Liquidate<'info> {
    /// The person initiating the liquidation. They pay the fees and receive the collateral.
    #[account(mut)]
    pub liquidator: Signer<'info>,

    /// The user account being liquidated. This is NOT a signer. We only need their address
    /// to derive the PDA for their user state account. This is a CRITICAL FIX.
    /// CHECK: The user_account is derived from this key, ensuring we liquidate the correct person.
    pub user_to_liquidate: AccountInfo<'info>,

    /// The state account of the user being liquidated.
    #[account(
        mut,
        seeds = [user_to_liquidate.key().as_ref()],
        bump
    )]
    pub user_account: Account<'info, User>,

    /// The mint of the asset that was BORROWED by the user (and is now being repaid by the liquidator).
    #[account(mut)]
    pub borrowed_mint: InterfaceAccount<'info, Mint>,

    /// The state account for the bank of the borrowed asset.
    #[account(mut, seeds = [borrowed_mint.key().as_ref()], bump)]
    pub borrowed_bank: Account<'info, Bank>,

    /// The vault for the borrowed asset, where the liquidator will send funds.
    #[account(mut, seeds = [b"treasury", borrowed_mint.key().as_ref()], bump)]
    pub borrowed_bank_token_account: InterfaceAccount<'info, TokenAccount>,

    /// The mint of the asset that was DEPOSITED as collateral (and is now being seized by the liquidator).
    pub collateral_mint: InterfaceAccount<'info, Mint>,

    /// The state account for the bank of the collateral asset.
    #[account(mut, seeds = [collateral_mint.key().as_ref()], bump)]
    pub collateral_bank: Account<'info, Bank>,
    
    /// The vault for the collateral asset, from which the liquidator will receive funds.
    #[account(mut, seeds = [b"treasury", collateral_mint.key().as_ref()], bump)]
    pub collateral_bank_token_account: InterfaceAccount<'info, TokenAccount>,
    
    /// The liquidator's token account for the BORROWED asset (where they send from).
    #[account(
        mut,
        associated_token::mint = borrowed_mint,
        associated_token::authority = liquidator,
    )]
    pub liquidator_borrowed_token_account: InterfaceAccount<'info, TokenAccount>,

    /// The liquidator's token account for the COLLATERAL asset (where they receive to).
    #[account(
        init_if_needed,
        payer = liquidator,
        associated_token::mint = collateral_mint,
        associated_token::authority = liquidator,
    )]
    pub liquidator_collateral_token_account: InterfaceAccount<'info, TokenAccount>,
    
    /// Pyth price feed account for valuing assets.
    pub price_update: Account<'info, PriceUpdateV2>,
    
    // Standard required programs
    pub token_program: Interface<'info, TokenInterface>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}


//================================================================
// Instruction Logic for Processing a Liquidation
//================================================================
pub fn process_liquidate(ctx: Context<Liquidate>) -> Result<()> {
    let user = &mut ctx.accounts.user_account;
    let price_update = &ctx.accounts.price_update;
    let clock = Clock::get()?;

    // --- 1. Perform Health Check ---
    // First, we must verify that the user's position is actually unhealthy and eligible for liquidation.
    msg!("Performing health check for user: {}", user.key());

    // Get prices for all assets involved.
    let sol_price = price_update.get_price_no_older_than(&clock, 60, &get_feed_id_from_hex(SOL_USD_FEED_ID)?)?;
    let usdc_price = price_update.get_price_no_older_than(&clock, 60, &get_feed_id_from_hex(USDC_USD_FEED_ID)?)?;

    // A. Calculate the total USD value of the user's DEBT.
    let total_debt_value = (sol_price.price as u128 * user.borrowed_sol as u128)
        .checked_add(usdc_price.price as u128 * user.borrowed_usdc as u128)
        .ok_or(ErrorCode::MathOverflow)?;

    // B. Calculate the total USD value of the user's COLLATERAL.
    let total_collateral_value = (sol_price.price as u128 * user.deposited_sol as u128)
        .checked_add(usdc_price.price as u128 * user.deposited_usdc as u128)
        .ok_or(ErrorCode::MathOverflow)?;

    // C. Apply the liquidation threshold to the collateral value.
    let weighted_collateral_value = total_collateral_value
        .checked_mul(ctx.accounts.collateral_bank.liquidation_threshold as u128).ok_or(ErrorCode::MathOverflow)?
        .checked_div(100).ok_or(ErrorCode::MathOverflow)?; // For percentage
    
    // D. The Health Check: If weighted collateral is still greater than or equal to the debt, revert.
    if weighted_collateral_value >= total_debt_value {
        return err!(ErrorCode::PositionHealthy);
    }
    msg!("Health check passed. Position is undercollateralized.");

    // --- 2. Calculate Liquidation Amounts in Native Tokens ---
    // This part is critical. We calculate everything in USD value first, then convert back to
    // the native token amounts for the actual transfers.

    // A. Determine the USD value of the debt to be repaid, capped by the close factor.
    let repay_value_usd = total_debt_value
        .checked_mul(ctx.accounts.borrowed_bank.liquidation_close_factor as u128).ok_or(ErrorCode::MathOverflow)?
        .checked_div(100).ok_or(ErrorCode::MathOverflow)?;

    // B. Convert the repay USD value back into the native amount of the BORROWED token.
    let (borrowed_token_price, borrowed_token_decimals) = match ctx.accounts.borrowed_mint.key() {
        key if key == USDC_MINT_ADDRESS.parse().unwrap() => (usdc_price.price, ctx.accounts.borrowed_mint.decimals),
        key if key == SOL_MINT_ADDRESS.parse().unwrap() => (sol_price.price, ctx.accounts.borrowed_mint.decimals),
        _ => return err!(ErrorCode::UnsupportedAsset),
    };
    let repay_amount_native = repay_value_usd.checked_div(borrowed_token_price as u128).ok_or(ErrorCode::MathOverflow)? as u64;

    // C. Determine the USD value of the collateral to be seized (repaid value + bonus).
    let seize_value_usd = repay_value_usd
        .checked_mul(100 + ctx.accounts.collateral_bank.liquidation_bonus as u128).ok_or(ErrorCode::MathOverflow)?
        .checked_div(100).ok_or(ErrorCode::MathOverflow)?;
    
    // D. Convert the seize USD value back into the native amount of the COLLATERAL token.
    let (collateral_token_price, collateral_token_decimals) = match ctx.accounts.collateral_mint.key() {
        key if key == USDC_MINT_ADDRESS.parse().unwrap() => (usdc_price.price, ctx.accounts.collateral_mint.decimals),
        key if key == SOL_MINT_ADDRESS.parse().unwrap() => (sol_price.price, ctx.accounts.collateral_mint.decimals),
        _ => return err!(ErrorCode::UnsupportedAsset),
    };
    let seize_amount_native = seize_value_usd.checked_div(collateral_token_price as u128).ok_or(ErrorCode::MathOverflow)? as u64;

    // --- 3. Perform CPI Transfers ---
    // A. Liquidator repays the user's debt to the bank.
    token_interface::transfer_checked(
        CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            TransferChecked {
                from: ctx.accounts.liquidator_borrowed_token_account.to_account_info(),
                mint: ctx.accounts.borrowed_mint.to_account_info(),
                to: ctx.accounts.borrowed_bank_token_account.to_account_info(),
                authority: ctx.accounts.liquidator.to_account_info(),
            },
        ),
        repay_amount_native,
        borrowed_token_decimals,
    )?;

    // B. Liquidator seizes discounted collateral from the bank's vault.
    let collateral_mint_key = ctx.accounts.collateral_mint.key();
    let signer_seeds: &[&[&[u8]]] = &[&[b"treasury", collateral_mint_key.as_ref(), &[ctx.bumps.collateral_bank_token_account]]];
    token_interface::transfer_checked(
        CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            TransferChecked {
                from: ctx.accounts.collateral_bank_token_account.to_account_info(),
                mint: ctx.accounts.collateral_mint.to_account_info(),
                to: ctx.accounts.liquidator_collateral_token_account.to_account_info(),
                authority: ctx.accounts.collateral_bank_token_account.to_account_info(),
            },
        ).with_signer(signer_seeds),
        seize_amount_native,
        collateral_token_decimals,
    )?;

    // --- 4. Update All State Accounts (CRITICAL) ---
    // This is the accounting that was missing from the original code.

    // Calculate shares to burn for both debt and collateral
    let shares_repaid = (repay_amount_native as u128 * ctx.accounts.borrowed_bank.total_borrow_shares as u128)
        .checked_div(ctx.accounts.borrowed_bank.total_borrows as u128).ok_or(ErrorCode::MathOverflow)? as u64;
    let shares_seized = (seize_amount_native as u128 * ctx.accounts.collateral_bank.total_deposit_shares as u128)
        .checked_div(ctx.accounts.collateral_bank.total_deposits as u128).ok_or(ErrorCode::MathOverflow)? as u64;

    // Update the state of the BORROWED bank
    let borrowed_bank = &mut ctx.accounts.borrowed_bank;
    borrowed_bank.total_borrows = borrowed_bank.total_borrows.checked_sub(repay_amount_native).ok_or(ErrorCode::MathOverflow)?;
    borrowed_bank.total_borrow_shares = borrowed_bank.total_borrow_shares.checked_sub(shares_repaid).ok_or(ErrorCode::MathOverflow)?;

    // Update the state of the COLLATERAL bank
    let collateral_bank = &mut ctx.accounts.collateral_bank;
    collateral_bank.total_deposits = collateral_bank.total_deposits.checked_sub(seize_amount_native).ok_or(ErrorCode::MathOverflow)?;
    collateral_bank.total_deposit_shares = collateral_bank.total_deposit_shares.checked_sub(shares_seized).ok_or(ErrorCode::MathOverflow)?;
    
    // Update the liquidated USER's state
    match ctx.accounts.borrowed_mint.key() {
        key if key == USDC_MINT_ADDRESS.parse().unwrap() => {
            user.borrowed_usdc = user.borrowed_usdc.checked_sub(repay_amount_native).ok_or(ErrorCode::MathOverflow)?;
            user.borrowed_usdc_shares = user.borrowed_usdc_shares.checked_sub(shares_repaid).ok_or(ErrorCode::MathOverflow)?;
        },
        key if key == SOL_MINT_ADDRESS.parse().unwrap() => {
            user.borrowed_sol = user.borrowed_sol.checked_sub(repay_amount_native).ok_or(ErrorCode::MathOverflow)?;
            user.borrowed_sol_shares = user.borrowed_sol_shares.checked_sub(shares_repaid).ok_or(ErrorCode::MathOverflow)?;
        },
        _ => return err!(ErrorCode::UnsupportedAsset),
    }

    match ctx.accounts.collateral_mint.key() {
        key if key == USDC_MINT_ADDRESS.parse().unwrap() => {
            user.deposited_usdc = user.deposited_usdc.checked_sub(seize_amount_native).ok_or(ErrorCode::MathOverflow)?;
            user.deposited_usdc_shares = user.deposited_usdc_shares.checked_sub(shares_seized).ok_or(ErrorCode::MathOverflow)?;
        },
        key if key == SOL_MINT_ADDRESS.parse().unwrap() => {
            user.deposited_sol = user.deposited_sol.checked_sub(seize_amount_native).ok_or(ErrorCode::MathOverflow)?;
            user.deposited_sol_shares = user.deposited_sol_shares.checked_sub(shares_seized).ok_or(ErrorCode::MathOverflow)?;
        },
        _ => return err!(ErrorCode::UnsupportedAsset),
    }

    msg!("Liquidation successful!");
    Ok(())
}
