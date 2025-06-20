use anchor_lang::prelude::*;
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token_interface::{self, Mint, TokenAccount, TokenInterface, TransferChecked};
use pyth_solana_receiver_sdk::price_update::{self, get_feed_id_from_hex, PriceUpdateV2};
use crate::state::*; // Assumes your Bank, User, etc., structs are here
use crate::error::ErrorCode; // Assumes your custom errors are here
use crate::constants::{SOL_USD_FEED_ID, USDC_USD_FEED_ID, SOL_MINT_ADDRESS}; // Assumes you have these constants defined

//================================================================
// Accounts Struct for the Borrow Instruction
//================================================================
#[derive(Accounts)]
pub struct Borrow<'info> {
    /// The user initiating the borrow, who will receive the tokens and pay for the transaction.
    #[account(mut)]
    pub signer: Signer<'info>,

    /// The Mint account of the token the user wants TO BORROW.
    pub mint_to_borrow: InterfaceAccount<'info, Mint>,

    /// The bank's state account for the asset being borrowed. This is crucial for
    /// getting the correct rules (like max_ltv) for this specific lending market.
    #[account(
        mut,
        seeds = [mint_to_borrow.key().as_ref()],
        bump,
    )]
    pub bank: Account<'info, Bank>,

    /// The bank's token vault for the asset being borrowed. This is the PDA account
    /// FROM WHICH tokens will be transferred to the user.
    #[account(
        mut,
        seeds = [b"treasury", mint_to_borrow.key().as_ref()],
        bump,
    )]
    pub bank_token_account: InterfaceAccount<'info, TokenAccount>,

    /// The user's state account, which tracks their total portfolio of deposits and borrows.
    #[account(
        mut,
        seeds = [signer.key().as_ref()],
        bump,
    )]
    pub user_account: Account<'info, User>,

    /// The user's Associated Token Account (ATA) where the borrowed tokens will be sent.
    /// Anchor creates this account if it doesn't exist (`init_if_needed`).
    #[account(
        init_if_needed,
        payer = signer,
        associated_token::mint = mint_to_borrow,
        associated_token::authority = signer,
    )]
    pub user_token_account: InterfaceAccount<'info, TokenAccount>,
    
    /// The Pyth PriceUpdateV2 account, which contains recent price feed data.
    /// This is crucial for getting real-time asset prices to value collateral.
    /// Note: In a real app, you might need to pass multiple price feed accounts
    /// if they don't all fit in one transaction.
    pub price_update: Account<'info, PriceUpdateV2>,

    /// The SPL Token Program (or the new Token-2022 Interface).
    pub token_program: Interface<'info, TokenInterface>,

    /// The Associated Token Program, needed for the `init_if_needed` constraint.
    pub associated_token_program: Program<'info, AssociatedToken>,
    
    /// The System Program, required by Anchor.
    pub system_program: Program<'info, System>,
}


//================================================================
// Instruction Logic for Processing a Borrow
//================================================================
pub fn process_borrow(ctx: Context<Borrow>, amount: u64) -> Result<()> {
    // --- 1. Security Check ---
    if amount == 0 {
        return err!(ErrorCode::ZeroAmount);
    }
    
    let user = &mut ctx.accounts.user_account;
    let bank = &mut ctx.accounts.bank;
    let price_update = &ctx.accounts.price_update;
    let clock = Clock::get()?;

    // --- 2. Calculate Total Collateral Value (Cross-Collateral Logic) ---
    // This section correctly calculates the total USD value of ALL assets the user has deposited.
    msg!("Calculating total collateral value...");

    // Get the price of SOL.
    let sol_feed_id = get_feed_id_from_hex(SOL_USD_FEED_ID)?;
    let sol_price = price_update.get_price_no_older_than(&clock, 60, &sol_feed_id)?;
    
    // Get the price of USDC.
    let usdc_feed_id = get_feed_id_from_hex(USDC_USD_FEED_ID)?;
    let usdc_price = price_update.get_price_no_older_than(&clock, 60, &usdc_feed_id)?;

    // Calculate the USD value of the user's SOL deposits.
    let sol_collateral_value = (sol_price.price as u128)
        .checked_mul(user.deposited_sol as u128)
        .ok_or(ErrorCode::MathOverflow)?;

    // Calculate the USD value of the user's USDC deposits.
    let usdc_collateral_value = (usdc_price.price as u128)
        .checked_mul(user.deposited_usdc as u128)
        .ok_or(ErrorCode::MathOverflow)?;
    
    // Sum the value of all assets to get the total collateral value.
    let total_collateral_value = sol_collateral_value
        .checked_add(usdc_collateral_value)
        .ok_or(ErrorCode::MathOverflow)?;

    msg!("Total Collateral Value (USD cents equivalent): {}", total_collateral_value);

    // --- 3. Calculate Borrowing Power ---
    // This calculates the maximum USD value the user is allowed to borrow based on their
    // total collateral and the bank's Max Loan-to-Value (LTV) ratio.
    let borrowable_usd_value = total_collateral_value
        .checked_mul(bank.max_ltv as u128) // e.g., 75
        .ok_or(ErrorCode::MathOverflow)?
        .checked_div(100) // for percentage -> e.g., 75 / 100 = 0.75
        .ok_or(ErrorCode::MathOverflow)?;
    
    msg!("Max Borrowable Value (USD cents equivalent): {}", borrowable_usd_value);

    // --- 4. Calculate Requested Borrow Value ---
    // This determines the USD value of the tokens the user is asking to borrow right now.
    let requested_borrow_asset_price: i64;
    match ctx.accounts.mint_to_borrow.key() {
        key if key == usdc_price.get_price_unchecked().price_expo => {
            requested_borrow_asset_price = usdc_price.get_price_unchecked().price;
        }
        key if key == SOL_MINT_ADDRESS.parse().unwrap() => { // Assumes wSOL mint
            requested_borrow_asset_price = sol_price.price;
        }
        _ => return err!(ErrorCode::UnsupportedAsset) // Strict check for supported assets.
    }

    let requested_borrow_value = (requested_borrow_asset_price as u128)
        .checked_mul(amount as u128)
        .ok_or(ErrorCode::MathOverflow)?;

    // --- 5. The Final Check: Collateral vs. Borrow ---
    if borrowable_usd_value < requested_borrow_value {
        return err!(ErrorCode::InsufficientCollateral);
    }
    
    // --- 6. Transfer Tokens to User (CPI) ---
    // The program signs using its PDA seeds to authorize the transfer FROM the bank's vault.
    let mint_key = ctx.accounts.mint_to_borrow.key();
    let signer_seeds: &[&[&[u8]]] = &[
        &[
            b"treasury",
            mint_key.as_ref(),
            &[ctx.bumps.bank_token_account], // The bump seed for the vault PDA
        ],
    ];
    
    let cpi_accounts = TransferChecked {
        from: ctx.accounts.bank_token_account.to_account_info(),
        mint: ctx.accounts.mint_to_borrow.to_account_info(),
        to: ctx.accounts.user_token_account.to_account_info(),
        authority: ctx.accounts.bank_token_account.to_account_info(), // The PDA is the authority
    };
    let cpi_program = ctx.accounts.token_program.to_account_info();
    let cpi_ctx = CpiContext::new(cpi_program, cpi_accounts).with_signer(signer_seeds);

    token_interface::transfer_checked(cpi_ctx, amount, ctx.accounts.mint_to_borrow.decimals)?;

    // --- 7. Update Bank and User State (Correct Accounting) ---
    // This logic correctly calculates borrow shares and adds them to the user's LIABILITIES.
    let users_borrow_shares: u64;
    if bank.total_borrows == 0 || bank.total_borrow_shares == 0 {
        users_borrow_shares = amount;
    } else {
        users_borrow_shares = (amount as u128)
            .checked_mul(bank.total_borrow_shares as u128)
            .ok_or(ErrorCode::MathOverflow)?
            .checked_div(bank.total_borrows as u128)
            .ok_or(ErrorCode::MathOverflow)? as u64;
    }

    // Update the bank's global state.
    bank.total_borrows = bank.total_borrows.checked_add(amount).ok_or(ErrorCode::MathOverflow)?;
    bank.total_borrow_shares = bank.total_borrow_shares.checked_add(users_borrow_shares).ok_or(ErrorCode::MathOverflow)?;

    // Update the user's specific debt accounts.
    match ctx.accounts.mint_to_borrow.key() {
        key if key == usdc_price.get_price_unchecked().price_expo => {
            user.borrowed_usdc = user.borrowed_usdc.checked_add(amount).ok_or(ErrorCode::MathOverflow)?;
            user.borrowed_usdc_shares = user.borrowed_usdc_shares.checked_add(users_borrow_shares).ok_or(ErrorCode::MathOverflow)?;
        }
        key if key == SOL_MINT_ADDRESS.parse().unwrap() => {
            user.borrowed_sol = user.borrowed_sol.checked_add(amount).ok_or(ErrorCode::MathOverflow)?;
            user.borrowed_sol_shares = user.borrowed_sol_shares.checked_add(users_borrow_shares).ok_or(ErrorCode::MathOverflow)?;
        }
        _ => return err!(ErrorCode::UnsupportedAsset) // Should be unreachable, but good practice.
    }

    // Update timestamps.
    bank.last_updated = clock.unix_timestamp;
    user.last_updated = clock.unix_timestamp;

    msg!("Borrow successful. Amount: {}, Shares: {}", amount, users_borrow_shares);
    
    Ok(())
}