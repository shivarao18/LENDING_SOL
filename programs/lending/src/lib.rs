use anchor_lang::prelude::*;

declare_id!("CUVw2rY1d7YSHL7WGXjhzwVnogbcr6i8zSmdwcdRmUYC");

#[program]
pub mod lending {
    use super::*;

    pub fn initialize(ctx: Context<Initialize>) -> Result<()> {
        msg!("Greetings from: {:?}", ctx.program_id);
        Ok(())
    }
}

#[derive(Accounts)]
pub struct Initialize {}
