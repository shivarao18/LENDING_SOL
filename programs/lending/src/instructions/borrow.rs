use anchor_lang::prelude::*;
use anchor_spl::associated_token::AssociatedToken;
// Using token_interface allows for compatibility with both SPL Token and Token-2022
use anchor_spl::token_interface::{self, Mint, TokenAccount, TokenInterface, TransferChecked};
use crate::state::*; // Assuming your Bank and User structs are in here

//================================================================
// Accounts Struct for the Deposit Instruction
//================================================================
#[derive(Accounts)]
pub struct Deposit<'info> {
    /// The user making the deposit, who is the authority and will pay for the transaction.
    #[account(mut)]
    pub signer: Signer<'info>,

    /// The Mint account of the token being deposited (e.g., USDC, wSOL).
    /// This is used to validate the token accounts and for CPI calls.
    pub mint: InterfaceAccount<'info, Mint>,

    /// The bank's state account. We use the mint's address as a seed to ensure
    /// we are depositing into the correct bank for the given asset.
    /// It must be mutable because we will update its total deposits and shares.
    #[account(
        mut,
        seeds = [mint.key().as_ref()],
        bump,
    )]
    pub bank: Account<'info, Bank>,

    /// The bank's token vault (a Program-Owned Token Account). This is where the
    /// actual tokens from the user will be transferred. It is a PDA seeded
    /// with "treasury" and the mint's address to make it unique for this bank.
    #[account(
        mut,
        seeds = [b"treasury", mint.key().as_ref()],
        bump,
    )]
    pub bank_token_account: InterfaceAccount<'info, TokenAccount>,

    /// The user's state account, which tracks their deposits and shares.
    /// It's a PDA seeded with the user's public key, making it unique per user.
    /// Needs to be mutable to update the user's balances.
    #[account(
        mut,
        seeds = [signer.key().as_ref()],
        bump,
    )]
    pub user_account: Account<'info, User>,

    /// The user's Associated Token Account (ATA) for the token being deposited.
    /// This is where the tokens will be transferred FROM. Anchor's constraints
    /// ensure this ATA belongs to the `signer` and is for the correct `mint`.
    #[account(
        mut,
        associated_token::mint = mint,
        associated_token::authority = signer,
    )]
    pub user_token_account: InterfaceAccount<'info, TokenAccount>,

    /// The SPL Token Program (or the new Token-2022 Interface).
    /// Required for making the `transfer_checked` CPI call.
    pub token_program: Interface<'info, TokenInterface>,

    /// The Associated Token Program, needed to validate the user's ATA.
    pub associated_token_program: Program<'info, AssociatedToken>,

    /// The System Program, required by Anchor for account creation and management.
    pub system_program: Program<'info, System>,
}

//================================================================
// Instruction Logic for Processing a Deposit
//================================================================
pub fn process_deposit(ctx: Context<Deposit>, amount: u64) -> Result<()> {
    // --- 1. Security Check ---
    // Ensure the user is not trying to deposit zero, which could cause issues.
    if amount == 0 {
        // You can define custom errors for more specific feedback.
        return err!(ErrorCode::ZeroAmount);
    }

    // --- 2. Transfer Tokens via CPI ---
    // This section creates a Cross-Program Invocation (CPI) to the official
    // SPL Token Program to securely transfer tokens from the user's account
    // to the bank's vault.
    let transfer_cpi_accounts = TransferChecked {
        from: ctx.accounts.user_token_account.to_account_info(),
        mint: ctx.accounts.mint.to_account_info(),
        to: ctx.accounts.bank_token_account.to_account_info(),
        authority: ctx.accounts.signer.to_account_info(),
    };
    let cpi_program = ctx.accounts.token_program.to_account_info();
    let cpi_ctx = CpiContext::new(cpi_program, transfer_cpi_accounts);

    // `transfer_checked` is safer than `transfer` because it requires the `decimals`
    // parameter, preventing potential token scaling attacks.
    token_interface::transfer_checked(cpi_ctx, amount, ctx.accounts.mint.decimals)?;

    // --- 3. Calculate Deposit Shares ---
    // This is the core logic for a lending protocol. We mint "shares" that represent
    // a user's claim on the underlying assets in the bank. This system ensures
    // that interest earned by the bank is distributed proportionally to all depositors.
    let bank = &mut ctx.accounts.bank;
    let users_shares: u64;

    if bank.total_deposits == 0 || bank.total_deposit_shares == 0 {
        // CASE A: The bank is empty (first depositor ever for this asset).
        // The share price is initialized at 1:1. 1 token = 1 share.
        users_shares = amount;
    } else {
        // CASE B: The bank already has deposits.
        // We calculate the number of shares to mint based on the current ratio of
        // shares to tokens. This prevents diluting the value for existing depositors.
        // Formula: new_shares = (amount_to_deposit * total_shares) / total_tokens
        //
        // We use u128 for the intermediate multiplication to prevent arithmetic overflow,
        // which can happen if `amount` and `total_deposit_shares` are both large.
        users_shares = (amount as u128)
            .checked_mul(bank.total_deposit_shares as u128)
            .unwrap() // Use .ok_or(ErrorCode::MathOverflow)? for better error handling
            .checked_div(bank.total_deposits as u128)
            .unwrap() as u64;
    }

    // --- 4. Update User and Bank State ---
    let user = &mut ctx.accounts.user_account;

    // The logic below assumes the User struct has specific fields like `deposited_usdc`.
    // A more scalable design might use a Map or a Vec of structs, but this is clear
    // for a tutorial.
    match ctx.accounts.mint.key() {
        // A placeholder for the actual USDC mint address on mainnet/devnet
        key if key == pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v") => {
            user.deposited_usdc = user.deposited_usdc.checked_add(amount).unwrap();
            user.deposited_usdc_shares = user.deposited_usdc_shares.checked_add(users_shares).unwrap();
        }
        // A placeholder for the wSOL mint address
        key if key == pubkey!("So11111111111111111111111111111111111111112") => {
            user.deposited_sol = user.deposited_sol.checked_add(amount).unwrap();
            user.deposited_sol_shares = user.deposited_sol_shares.checked_add(users_shares).unwrap();
        }
        _ => {
            // It's good practice to return an error if the asset is not supported.
            return err!(ErrorCode::UnsupportedAsset);
        }
    }

    // Finally, update the bank's global state totals.
    bank.total_deposits = bank.total_deposits.checked_add(amount).unwrap();
    bank.total_deposit_shares = bank.total_deposit_shares.checked_add(users_shares).unwrap();

    // Update the timestamp to reflect recent activity. Useful for interest calculations.
    bank.last_updated = Clock::get()?.unix_timestamp;
    user.last_updated = Clock::get()?.unix_timestamp;

    msg!("Deposit successful. Amount: {}, Shares minted: {}", amount, users_shares);

    Ok(())
}

/
