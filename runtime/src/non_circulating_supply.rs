use {
    crate::bank::Bank,
    log::*,
    solana_account::ReadableAccount,
    solana_accounts_db::{
        accounts_index::{AccountIndex, IndexKey},
        accounts_scan::ScanResult,
    },
    solana_pubkey::Pubkey,
    solana_stake_interface::{self as stake, state::StakeStateV2},
    std::collections::HashSet,
};

pub struct NonCirculatingSupply {
    pub lamports: u64,
    pub accounts: Vec<Pubkey>,
}

pub fn calculate_non_circulating_supply(bank: &Bank) -> ScanResult<NonCirculatingSupply> {
    debug!("Updating Bank supply, epoch: {}", bank.epoch());
    let mut non_circulating_accounts_set: HashSet<Pubkey> = HashSet::new();

    for key in non_circulating_accounts() {
        non_circulating_accounts_set.insert(key);
    }
    let withdraw_authority_list = withdraw_authority();

    let clock = bank.clock();
    let stake_accounts = if bank
        .rc
        .accounts
        .accounts_db
        .account_indexes
        .contains(&AccountIndex::ProgramId)
    {
        bank.get_filtered_indexed_accounts(
            &IndexKey::ProgramId(stake::program::id()),
            // The program-id account index checks for Account owner on inclusion. However, due to
            // the current AccountsDb implementation, an account may remain in storage as a
            // zero-lamport Account::Default() after being wiped and reinitialized in later
            // updates. We include the redundant filter here to avoid returning these accounts.
            |account| account.owner() == &stake::program::id(),
            None,
        )?
    } else {
        bank.get_program_accounts(&stake::program::id())?
    };

    for (pubkey, account) in stake_accounts.iter() {
        let stake_account = account
            .deserialize_data::<StakeStateV2>()
            .unwrap_or_default();
        match stake_account {
            StakeStateV2::Initialized(meta)
                if (meta.lockup.is_in_force(&clock, None)
                    || withdraw_authority_list.contains(&meta.authorized.withdrawer)) =>
            {
                non_circulating_accounts_set.insert(*pubkey);
            }
            StakeStateV2::Stake(meta, _stake, _stake_flags)
                if (meta.lockup.is_in_force(&clock, None)
                    || withdraw_authority_list.contains(&meta.authorized.withdrawer)) =>
            {
                non_circulating_accounts_set.insert(*pubkey);
            }
            _ => {}
        }
    }

    let lamports = non_circulating_accounts_set
        .iter()
        .map(|pubkey| bank.get_balance(pubkey))
        .sum();

    Ok(NonCirculatingSupply {
        lamports,
        accounts: non_circulating_accounts_set.into_iter().collect(),
    })
}

// Mainnet-beta accounts that should be considered non-circulating
pub fn non_circulating_accounts() -> Vec<Pubkey> {
    [
        solana_pubkey::pubkey!("9huDUZfxoJ7wGMTffUE7vh1xePqef7gyrLJu9NApncqA"),
        solana_pubkey::pubkey!("GK2zqSsXLA2rwVZk347RYhh6jJpRsCA69FjLW93ZGi3B"),
        solana_pubkey::pubkey!("CWeRmXme7LmbaUWTZWFLt6FMnpzLCHaQLuR2TdgFn4Lq"),
        solana_pubkey::pubkey!("HCV5dGFJXRrJ3jhDYA4DCeb9TEDTwGGYXtT3wHksu2Zr"),
        solana_pubkey::pubkey!("14FUT96s9swbmH7ZjpDvfEDywnAYy9zaNhv4xvezySGu"),
        solana_pubkey::pubkey!("HbZ5FfmKWNHC7uwk6TF1hVi6TCs7dtYfdjEcuPGgzFAg"),
        solana_pubkey::pubkey!("C7C8odR8oashR5Feyrq2tJKaXL18id1dSj2zbkDGL2C2"),
        solana_pubkey::pubkey!("Eyr9P5XsjK2NUKNCnfu39eqpGoiLFgVAv1LSQgMZCwiQ"),
        solana_pubkey::pubkey!("DE1bawNcRJB9rVm3buyMVfr8mBEoyyu73NBovf2oXJsJ"),
        solana_pubkey::pubkey!("CakcnaRDHka2gXyfbEd2d3xsvkJkqsLw2akB3zsN1D2S"),
        solana_pubkey::pubkey!("7Np41oeYqPefeNQEHSv1UDhYrehxin3NStELsSKCT4K2"),
        solana_pubkey::pubkey!("GdnSyH3YtwcxFvQrVVJMm1JhTS4QVX7MFsX56uJLUfiZ"),
        solana_pubkey::pubkey!("Mc5XB47H3DKJHym5RLa9mPzWv5snERsF3KNv5AauXK8"),
        solana_pubkey::pubkey!("7cvkjYAkUYs4W8XcXsca7cBrEGFeSUjeZmKoNBvEwyri"),
        solana_pubkey::pubkey!("AG3m2bAibcY8raMt4oXEGqRHwX4FWKPPJVjZxn1LySDX"),
        solana_pubkey::pubkey!("5XdtyEDREHJXXW1CTtCsVjJRjBapAwK78ZquzvnNVRrV"),
        solana_pubkey::pubkey!("6yKHERk8rsbmJxvMpPuwPs1ct3hRiP7xaJF2tvnGU6nK"),
        solana_pubkey::pubkey!("CHmdL15akDcJgBkY6BP3hzs98Dqr6wbdDC5p8odvtSbq"),
        solana_pubkey::pubkey!("FR84wZQy3Y3j2gWz6pgETUiUoJtreMEuWfbg6573UCj9"),
        solana_pubkey::pubkey!("5q54XjQ7vDx4y6KphPeE97LUNiYGtP55spjvXAWPGBuf"),
        solana_pubkey::pubkey!("3o6xgkJ9sTmDeQWyfj3sxwon18fXJB9PV5LDc8sfgR4a"),
        solana_pubkey::pubkey!("GumSE5HsMV5HCwBTv2D2D81yy9x17aDkvobkqAfTRgmo"),
        solana_pubkey::pubkey!("AzVV9ZZDxTgW4wWfJmsG6ytaHpQGSe1yz76Nyy84VbQF"),
        solana_pubkey::pubkey!("8CUUMKYNGxdgYio5CLHRHyzMEhhVRMcqefgE6dLqnVRK"),
        solana_pubkey::pubkey!("CQDYc4ET2mbFhVpgj41gXahL6Exn5ZoPcGAzSHuYxwmE"),
        solana_pubkey::pubkey!("5PLJZLJiRR9vf7d1JCCg7UuWjtyN9nkab9uok6TqSyuP"),
        solana_pubkey::pubkey!("7xJ9CLtEAcEShw9kW2gSoZkRWL566Dg12cvgzANJwbTr"),
        solana_pubkey::pubkey!("BuCEvc9ze8UoAQwwsQLy8d447C8sA4zeVtVpc6m5wQeS"),
        solana_pubkey::pubkey!("8ndGYFjav6NDXvzYcxs449Aub3AxYv4vYpk89zRDwgj7"),
        solana_pubkey::pubkey!("8W58E8JVJjH1jCy5CeHJQgvwFXTyAVyesuXRZGbcSUGG"),
        solana_pubkey::pubkey!("GNiz4Mq886bTNDT3pijGsu2gbw6it7sqrwncro45USeB"),
        solana_pubkey::pubkey!("GhsotwFMH6XUrRLJCxcx62h7748N2Uq8mf87hUGkmPhg"),
        solana_pubkey::pubkey!("Fgyh8EeYGZtbW8sS33YmNQnzx54WXPrJ5KWNPkCfWPot"),
        solana_pubkey::pubkey!("8UVjvYyoqP6sqcctTso3xpCdCfgTMiv3VRh7vraC2eJk"),
        solana_pubkey::pubkey!("BhvLngiqqKeZ8rpxch2uGjeCiC88zzewoWPRuoxpp1aS"),
        solana_pubkey::pubkey!("63DtkW7zuARcd185EmHAkfF44bDcC2SiTSEj2spLP3iA"),
        solana_pubkey::pubkey!("GvpCiTgq9dmEeojCDBivoLoZqc4AkbUDACpqPMwYLWKh"),
        solana_pubkey::pubkey!("7Y8smnoUrYKGGuDq2uaFKVxJYhojgg7DVixHyAtGTYEV"),
        solana_pubkey::pubkey!("DUS1KxwUhUyDKB4A81E8vdnTe3hSahd92Abtn9CXsEcj"),
        solana_pubkey::pubkey!("F9MWFw8cnYVwsRq8Am1PGfFL3cQUZV37mbGoxZftzLjN"),
        solana_pubkey::pubkey!("8vqrX3H2BYLaXVintse3gorPEM4TgTwTFZNN1Fm9TdYs"),
        solana_pubkey::pubkey!("CUageMFi49kzoDqtdU8NvQ4Bq3sbtJygjKDAXJ45nmAi"),
        solana_pubkey::pubkey!("5smrYwb1Hr2T8XMnvsqccTgXxuqQs14iuE8RbHFYf2Cf"),
        solana_pubkey::pubkey!("xQadXQiUTCCFhfHjvQx1hyJK6KVWr1w2fD6DT3cdwj7"),
        solana_pubkey::pubkey!("8DE8fqPfv1fp9DHyGyDFFaMjpopMgDeXspzoi9jpBJjC"),
        solana_pubkey::pubkey!("3itU5ME8L6FDqtMiRoUiT1F7PwbkTtHBbW51YWD5jtjm"),
        solana_pubkey::pubkey!("AsrYX4FeLXnZcrjcZmrASY2Eq1jvEeQfwxtNTxS5zojA"),
        solana_pubkey::pubkey!("8rT45mqpuDBR1vcnDc9kwP9DrZAXDR4ZeuKWw3u1gTGa"),
        solana_pubkey::pubkey!("nGME7HgBT6tAJN1f6YuCCngpqT5cvSTndZUVLjQ4jwA"),
        solana_pubkey::pubkey!("CzAHrrrHKx9Lxf6wdCMrsZkLvk74c7J2vGv8VYPUmY6v"),
        solana_pubkey::pubkey!("AzHQ8Bia1grVVbcGyci7wzueSWkgvu7YZVZ4B9rkL5P6"),
        solana_pubkey::pubkey!("FiWYY85b58zEEcPtxe3PuqzWPjqBJXqdwgZeqSBmT9Cn"),
        solana_pubkey::pubkey!("GpxpMVhrBBBEYbEJxdR62w3daWz444V7m6dxYDZKH77D"),
        solana_pubkey::pubkey!("3bTGcGB9F98XxnrBNftmmm48JGfPgi5sYxDEKiCjQYk3"),
        solana_pubkey::pubkey!("8pNBEppa1VcFAsx4Hzq9CpdXUXZjUXbvQwLX2K7QsCwb"),
        solana_pubkey::pubkey!("HKJgYGTTYYR2ZkfJKHbn58w676fKueQXmvbtpyvrSM3N"),
        solana_pubkey::pubkey!("3jnknRabs7G2V9dKhxd2KP85pNWXKXiedYnYxtySnQMs"),
        solana_pubkey::pubkey!("4sxwau4mdqZ8zEJsfryXq4QFYnMJSCp3HWuZQod8WU5k"),
        solana_pubkey::pubkey!("Fg12tB1tz8w6zJSQ4ZAGotWoCztdMJF9hqK8R11pakog"),
        solana_pubkey::pubkey!("GEWSkfWgHkpiLbeKaAnwvqnECGdRNf49at5nFccVey7c"),
        solana_pubkey::pubkey!("CND6ZjRTzaCFVdX7pSSWgjTfHZuhxqFDoUBqWBJguNoA"),
        solana_pubkey::pubkey!("2WWb1gRzuXDd5viZLQF7pNRR6Y7UiyeaPpaL35X6j3ve"),
        solana_pubkey::pubkey!("BUnRE27mYXN9p8H1Ay24GXhJC88q2CuwLoNU2v2CrW4W"),
        solana_pubkey::pubkey!("CsUqV42gVQLJwQsKyjWHqGkfHarxn9hcY4YeSjgaaeTd"),
        solana_pubkey::pubkey!("5khMKAcvmsFaAhoKkdg3u5abvKsmjUQNmhTNP624WB1F"),
        solana_pubkey::pubkey!("GpYnVDgB7dzvwSgsjQFeHznjG6Kt1DLBFYrKxjGU1LuD"),
        solana_pubkey::pubkey!("DQQGPtj7pphPHCLzzBuEyDDQByUcKGrsJdsH7SP3hAug"),
        solana_pubkey::pubkey!("FwfaykN7ACnsEUDHANzGHqTGQZMcGnUSsahAHUqbdPrz"),
        solana_pubkey::pubkey!("JCwT5Ygmq3VeBEbDjL8s8E82Ra2rP9bq45QfZE7Xyaq7"),
        solana_pubkey::pubkey!("H3Ni7vG1CsmJZdTvxF7RkAf9UM5qk4RsohJsmPvtZNnu"),
        solana_pubkey::pubkey!("CVgyXrbEd1ctEuvq11QdpnCQVnPit8NLdhyqXQHLprM2"),
        solana_pubkey::pubkey!("EAJJD6nDqtXcZ4DnQb19F9XEz8y8bRDHxbWbahatZNbL"),
        solana_pubkey::pubkey!("6o5v1HC7WhBnLfRHp8mQTtCP2khdXXjhuyGyYEoy2Suy"),
        solana_pubkey::pubkey!("3ZrsTmNM6AkMcqFfv3ryfhQ2jMfqP64RQbqVyAaxqhrQ"),
        solana_pubkey::pubkey!("6zw7em7uQdmMpuS9fGz8Nq9TLHa5YQhEKKwPjo5PwDK4"),
        solana_pubkey::pubkey!("CuatS6njAcfkFHnvai7zXCs7syA9bykXWsDCJEWfhjHG"),
        solana_pubkey::pubkey!("Hz9nydgN1k15wnwffKX7CSmZp4VFTnTwLXAEdomFGNXy"),
        solana_pubkey::pubkey!("Ep5Y58PaSyALPrdFxDVAdfKtVdP55vApvsWjb3jSmXsG"),
        solana_pubkey::pubkey!("EziVYi3Sv5kJWxmU77PnbrT8jmkVuqwdiFLLzZpLVEn7"),
        solana_pubkey::pubkey!("H1rt8KvXkNhQExTRfkY8r9wjZbZ8yCih6J4wQ5Fz9HGP"),
        solana_pubkey::pubkey!("6nN69B4uZuESZYxr9nrLDjmKRtjDZQXrehwkfQTKw62U"),
        solana_pubkey::pubkey!("Hm9JW7of5i9dnrboS8pCUCSeoQUPh7JsP1rkbJnW7An4"),
        solana_pubkey::pubkey!("5D5NxsNVTgXHyVziwV7mDFwVDS6voaBsyyGxUbhQrhNW"),
        solana_pubkey::pubkey!("EMAY24PrS6rWfvpqffFCsTsFJypeeYYmtUc26wdh3Wup"),
        solana_pubkey::pubkey!("Br3aeVGapRb2xTq17RU2pYZCoJpWA7bq6TKBCcYtMSmt"),
        solana_pubkey::pubkey!("BUjkdqUuH5Lz9XzcMcR4DdEMnFG6r8QzUMBm16Rfau96"),
        solana_pubkey::pubkey!("Es13uD2p64UVPFpEWfDtd6SERdoNR2XVgqBQBZcZSLqW"),
        solana_pubkey::pubkey!("AVYpwVou2BhdLivAwLxKPALZQsY7aZNkNmGbP2fZw7RU"),
        solana_pubkey::pubkey!("DrKzW5koKSZp4mg4BdHLwr72MMXscd2kTiWgckCvvPXz"),
        solana_pubkey::pubkey!("9hknftBZAQL4f48tWfk3bUEV5YSLcYYtDRqNmpNnhCWG"),
        solana_pubkey::pubkey!("GLUmCeJpXB8veNcchPwibkRYwCwvQbKodex5mEjrgToi"),
        solana_pubkey::pubkey!("9S2M3UYPpnPZTBtbcUvehYmiWFK3kBhwfzV2iWuwvaVy"),
        solana_pubkey::pubkey!("HUAkU5psJXZuw54Lrg1ksbXzHv2fzczQ9sNbmisVMeJU"),
        solana_pubkey::pubkey!("GK8R4uUmrawcREZ5xJy5dAzVV5V7aFvYg77id37pVTK"),
        solana_pubkey::pubkey!("4vuWt1oHRqLMhf8Nv1zyEXZsYaeK7dipwrfKLoYU9Riq"),
        solana_pubkey::pubkey!("EMhn1U3TMimW3bvWYbPUvN2eZnCfsuBN4LGWhzzYhiWR"),
        solana_pubkey::pubkey!("BsKsunvENxAraBrL77UfAn1Gi7unVEmQAdCbhsjUN6tU"),
        solana_pubkey::pubkey!("CTvhdUVf8KNyMbyEdnvRrBCHJjBKtQwkbj6zwoqcEssG"),
        solana_pubkey::pubkey!("3fV2GaDKa3pZxyDcpMh5Vrh2FVAMUiWUKbYmnBFv8As3"),
        solana_pubkey::pubkey!("4pV47TiPzZ7SSBPHmgUvSLmH9mMSe8tjyPhQZGbi1zPC"),
        solana_pubkey::pubkey!("P8aKfWQPeRnsZtpBrwWTYzyAoRk74KMz56xc6NEpC4J"),
        solana_pubkey::pubkey!("HuqDWJodFhAEWh6aWdsDVUqsjRket5DYXMYyDYtD8hdN"),
        solana_pubkey::pubkey!("Ab1UcdsFXZVnkSt1Z3vcYU65GQk5MvCbs54SviaiaqHb"),
        solana_pubkey::pubkey!("Dc2oHxFXQaC2QfLStuU7txtD3U5HZ82MrCSGDooWjbsv"),
        solana_pubkey::pubkey!("3iPvAS4xdhYr6SkhVDHCLr7tJjMAFK4wvvHWJxFQVg15"),
        solana_pubkey::pubkey!("GmyW1nqYcrw7P7JqrcyP9ivU9hYNbrgZ1r5SYJJH41Fs"),
        solana_pubkey::pubkey!("E8jcgWvrvV7rwYHJThwfiBeQ8VAH4FgNEEMG9aAuCMAq"),
        solana_pubkey::pubkey!("CY7X5o3Wi2eQhTocLmUS6JSWyx1NinBfW7AXRrkRCpi8"),
        solana_pubkey::pubkey!("HQJtLqvEGGxgNYfRXUurfxV8E1swvCnsbC3456ik27HY"),
        solana_pubkey::pubkey!("9xbcBZoGYFnfJZe81EDuDYKUm8xGkjzW8z4EgnVhNvsv"),
        solana_pubkey::pubkey!("4sNBQyPbJCQyUimBueZkGWnLVqds4rWkm7eXyi9WskGU"),
        solana_pubkey::pubkey!("AZWdNvnZxJnbcT8ZzonpN19AZJadxPdUxSiCEDTJzu8L"),
        solana_pubkey::pubkey!("5kFTzLuM2VgFdb6x16smnY3JWoVdPxNZVFAqeVgjSTUP"),
        solana_pubkey::pubkey!("Gbz6wkNFus8SNEkWGNNENLv9NFwVvF1pWVDpaVKUWcMh"),
        solana_pubkey::pubkey!("AksPzoA9DKCipgdhHjhUzQJe4iEniCvBoEfvayuFA3BN"),
        solana_pubkey::pubkey!("DVhs8YHWrvhhGxoefDNY9KotqtEEnjnSAK8MYGL2Q7X"),
        solana_pubkey::pubkey!("2bvGnYAPSV8pa1H3vRYr5tPAXktP4DkFACHfAgqyyfhd"),
        solana_pubkey::pubkey!("2xP8YQ3sVmfNPtGM17tZi7Lr4vsPUiN6mHLx42roazG5"),
        solana_pubkey::pubkey!("3p9ZxnrSFTkXVrT3KnYg2tT6asnysDApEFB5DRkdeAhB"),
        solana_pubkey::pubkey!("wRVP5MYuqP8HJ1Q8RCJ5NzUraL3DxCKPGMKSBd5iQH1"),
        solana_pubkey::pubkey!("513qFSVgmAQFBDsnyFCM1MrVKBrWiDgb4nXrdGsqa7Z4"),
        solana_pubkey::pubkey!("619qLS85ieR4qh7MGNLLZyLefN7hMi2FDVb5cmX1nisb"),
        solana_pubkey::pubkey!("768NcPfBJpFBtjDAbYZLFkej6ca1W5jeArsW5MtdF8S9"),
        solana_pubkey::pubkey!("EWAjC8a9VPbALSM3D6tGsbRfgDV48kRuHZPu8qtYSNDv"),
        solana_pubkey::pubkey!("H8wurbnaaXsgtrjqkNH1HhncUPUhTLAmKUHkwMeyqmfN"),
        solana_pubkey::pubkey!("HBfi37TwD4kMa1WrAWwXp3ZaFbZQ1g3XxWwNZs8QsCpY"),
        solana_pubkey::pubkey!("6B7mXMM6BixHvDpPAPLSKweLyCXcbtprkfsw3HfMUSjZ"),
        solana_pubkey::pubkey!("5TuV9WpmESXNfTNqasXVehoXpQy65WUHBbzgKPXPLwWx"),
        solana_pubkey::pubkey!("FB6VmiYFnVGp1uKXA3WbNsqg9neGYVpwYYiB9q8bFRrQ"),
        solana_pubkey::pubkey!("7Anoa4ZRiq8qaaiEnhmdpXyTEmBjZASXGfnQqVDynNie"),
        solana_pubkey::pubkey!("6dXeE5hS8bQKeqZsc18ewCyqHimnhCBAvVJBusnRqa2F"),
        solana_pubkey::pubkey!("Ds69ZQPb3D3aVPXdN5REyzALBrzJLdruJZ1cwyfyoEEx"),
        solana_pubkey::pubkey!("5HA8QV7tp59iNpfjs8f84LGUGX4imaynMFSKWHUcTrMT"),
        solana_pubkey::pubkey!("EPfiDzgbdgXdyqqwbYFMqU6Qvfx2J9Zf2J9noXBYYbbx"),
        solana_pubkey::pubkey!("DEGyTHFXmYuyANRDYRoEcShBXLonWNdBRpNzbFZBuzhY"),
        solana_pubkey::pubkey!("9xz1vZSWgY6TFPLZgPM2WJjDM1KiPXcALnjYFHRTkYiK"),
        solana_pubkey::pubkey!("6n1mSmsdGFCkEyyHe4wtgEigbwhiwYRszrerMW9YRyYF"),
        solana_pubkey::pubkey!("8rp9vcJG1Uwk25g9XfQNHysfGBYcHizVEJvavqiffB6P"),
        solana_pubkey::pubkey!("BVsJuJbhXmxPB9XPZrYMXs1SfuZuo998u1jQ9emVp8YD"),
        solana_pubkey::pubkey!("Fas9FrnngyVjdMZ6r8p8SsfyFDtHLMmrw1dyBxRWWVTF"),
        solana_pubkey::pubkey!("6Lfk4e6yxXGcFNMHue5u8KQM9hWYKjbdMFZ2n7SHHd6L"),
        solana_pubkey::pubkey!("6WuU6DWoM7ZpgUVL5TR97Fgose7vLgrv4Qvg1Yhqv8Xw"),
        solana_pubkey::pubkey!("6mXiGJbQbX726ZhrMSmbLCF81E8rkSfo1pvBWyn1asG5"),
        solana_pubkey::pubkey!("2UdDthRdtZKMjEx9oQYnzuLdWy3sa4XKeaqihdXkXFza"),
        solana_pubkey::pubkey!("33XwSCq2Ft5tTCsGK7kTyMCJ4hqufDZT2REFAWXmsia8"),
        solana_pubkey::pubkey!("B5tRZWRCTCLANkY5zLnxEXAn8S2exJD9EkH3cacEYqmE"),
        solana_pubkey::pubkey!("CfCvwXZk1sLSqzuoFx1K7KG1puMofgwHA3tSpcArVysX"),
        solana_pubkey::pubkey!("C4fATCN93YL5iYS3c2MrHeDmkMD6waHuBXLqsri5q5AF"),
        solana_pubkey::pubkey!("HbJUXNHXLzpucDr9wgjuoqAwK6XNKEcqi5h29MkkuSBQ"),
        solana_pubkey::pubkey!("G8EBx1Qo5W8731nmaBSGYyvU96onpEP5adrVSjt4vnLF"),
        solana_pubkey::pubkey!("4dc5ty7TNod2EQfx4bPnDofPbQk5cELcn3it47qnLizL"),
        solana_pubkey::pubkey!("4VCET97mE8ixxAPRYabM7S7dvkiAFZ8ZzEoH6kbmfYus"),
        solana_pubkey::pubkey!("DwqBsPgtAHqQUmRN3WF5YTuLKhK7dqzrb9eRJ4UVRNLf"),
        solana_pubkey::pubkey!("26FqcAbTLvr6cVXk2ktPu9hpeMYWukY7uUwNCtTwu7Wb"),
        solana_pubkey::pubkey!("B22zqCsZL6PDsi4fftmcjj5k7hFjhH1BM8Dcz4fXfFXK"),
        solana_pubkey::pubkey!("9ahLw5LFpeTuBykK3A5aiFZ1CidTY7z4YxG2kmQdSGon"),
        solana_pubkey::pubkey!("8M731ZthMdeVoMFGRaPnUBzR7Ze6qY4zEcQ7WALEAn5i"),
        solana_pubkey::pubkey!("A3aaVsuX7BfNCUQbuepQSaLs9KNB7c6djG6u4UuJzwwP"),
        solana_pubkey::pubkey!("DeXE7LTUqsC3B9kSACfFdW5TPG1eT9n1Z6gVB9dJNxqz"),
        solana_pubkey::pubkey!("BtvSeDfZNfA3VEH6baFprfx6JcS6JX8jdt6NffCxoJGV"),
        solana_pubkey::pubkey!("8o8trB6XdG8o4qo29De2mcooY5E3pW2rvMMkeBwJHkUD"),
        solana_pubkey::pubkey!("CSSxN8vQYqSjg5GV6ZUGSM65dXqhytLzANLpMrBZjLKP"),
        solana_pubkey::pubkey!("5A2yoxdGdC3B3QC39pkXpjx2ZPH4WGgie9exHQnhNz4J"),
        solana_pubkey::pubkey!("DFaHYDcuLmxx3vrJGCgw8sPehzBNjvbvZTjz7PgvYLcb"),
        solana_pubkey::pubkey!("43Cwv866ActL11JqVQXoFPiytUYA3tFXUaNg1zPsarn9"),
        solana_pubkey::pubkey!("3wzw3L4wd3B7DHocbQxHosQXEfqEz5AnkQHKBisGKLzY"),
        solana_pubkey::pubkey!("EnX9MvycxMGKFpMPJHgNo6kD2proVaorKQcSaMmguMEZ"),
        solana_pubkey::pubkey!("GLz4zxemfHJy3BnK7PjZ4uUgJoHtycpT3JrmTDYduVpg"),
        solana_pubkey::pubkey!("362rDzQwChHJ9ToZgLdv8wzFHTTsxsxzFLTZPwKY3CYg"),
        solana_pubkey::pubkey!("3FKwgoQWCz4i43Pbo3jdfsjzkgrkjnk44rDWh7A9J9Ee"),
        solana_pubkey::pubkey!("Cj53e3n1WX9dAUNX5PZpsc9xG9e7hXGnuJkKLJkNaf6v"),
        solana_pubkey::pubkey!("Amn9KQZY5kJj932b49pxwqZuuKfNWewrmNQtdMrRfdvf"),
        solana_pubkey::pubkey!("6spvvQJzFCcw5fVVdF1zKxCeq8Gj2yRFGgFuG16fYSMo"),
        solana_pubkey::pubkey!("H5PutDz8EgAoznaX7Bc8qSB6sPxD2Xho145H6MH6AcbV"),
        solana_pubkey::pubkey!("HNCooAiRiVeqGK9VSjxsvRsfr4weTcx14XhkvM7zDMgx"),
        solana_pubkey::pubkey!("AUqLkd6bR54NtXY9Zt4J2zZ7CJbhVHuDR95YFk5qahaX"),
        solana_pubkey::pubkey!("H35YRuMGE6WDPBLu3fDYLxV7tEcL2yJ7C97jDkw6NrBf"),
        solana_pubkey::pubkey!("EhN8bv8LCXnNAVju9ERGsCccmdBNmNTAESAHXE8PyBLR"),
        solana_pubkey::pubkey!("Ht5roME7Fjzt3dcEyf7UxYpPPAAdyTrnJ6rgEwQrNAFg"),
        solana_pubkey::pubkey!("5zHRch6mowPiobRQHYXZ1siRcthw4dYkFPuaNDQkK3EE"),
        solana_pubkey::pubkey!("AZkdkkHzSEbaZXpr7xBmt71AHj9c8iWE2oWYELm3nns5"),
        solana_pubkey::pubkey!("FThHNjNQYi7FLvP4KjeYf3LXjcN2LhyYHmza35ucqtDT"),
        solana_pubkey::pubkey!("6vHscAGjgbTfi1ooRVWGjSiUhHcUnLBceJyuKjpKrCYy"),
        solana_pubkey::pubkey!("AnbszMCtjEVZTquG3FQ68x1f1WK5B1Z6E7gtN2J6vD5a"),
        solana_pubkey::pubkey!("5aUknZidNKmAPxLMHoUeWsEdmXX8sGKvj8s71pHTDC7H"),
        solana_pubkey::pubkey!("7Xgby7EgwoRkhbQSdNJL9sXwpo3jddUhqB4zv3WuJB4m"),
        solana_pubkey::pubkey!("43KdfhYK6LUXX4LmyxfZ1KLaQrfpRe8vNwopNS7wXsUC"),
        solana_pubkey::pubkey!("FG6UGgT4VpCQxjwQsVy6woHeemwb5jP5u9kR5ry3twcK"),
        solana_pubkey::pubkey!("BDvLyqobZFubd7ESEJde6KqjbNoChkdj5aEiBdCqcN5r"),
        solana_pubkey::pubkey!("BtF4msVoQYJPrQUMwNtEXw58LVjux18ddVhC4XjbsdwX"),
        solana_pubkey::pubkey!("GbPK6cgUKso2ZGNV6GRwbp3hkdUzTVjsybYQMGDBvMFa"),
        solana_pubkey::pubkey!("4bWjzNek2m6P3Q8rKkq11C47TaoAwpj3ykk5yDCYLWr1"),
        solana_pubkey::pubkey!("FPzPT7YV5BHSTjYjqycXbwBsD4E5WsrZNPcCgcHhPkUx"),
        solana_pubkey::pubkey!("EbitXxcwjC1dzrGJ7sa3Y6RBzSM3wxvrxMaBncNBKEf2"),
        solana_pubkey::pubkey!("HMmHHmCmC3oo4G4QCjKvNxctSBQzRFDp2WQHZoXyrW6Q"),
        solana_pubkey::pubkey!("145FLxWrDG9gKifyJKSnkKNAUPhG6hxUMEiZWw6XeSMW"),
        solana_pubkey::pubkey!("FnXgwJZT7RMhpYEJFCXGLHp63bhVLfVn5d4aSVQDe19n"),
        solana_pubkey::pubkey!("5tDptzMGG79DLp9XV4fdcmiRNPQGCkfBJeo8EVCSYKw7"),
        solana_pubkey::pubkey!("9oBNYSARUMuH2swnxzbxCi827YfUnYWyWSAkrHz93h5y"),
        solana_pubkey::pubkey!("DWfXRxmUXKdSTji4S1nXncviTmrhWQwxHAQCcvjRLeWm"),
        solana_pubkey::pubkey!("EHUv6Hc9XGAb57C8iNCUuoE9xnJnRTFr9LaP7zbu8LNB"),
        solana_pubkey::pubkey!("CTqnGTKHrcg8AVK5pSwVzpA3mmSxz7Js9y7rEqCbjryH"),
        solana_pubkey::pubkey!("7PzsZ3ApLcfQosSr2B1C9YV1GNNgriZVFyqzDpeHoAD"),
        solana_pubkey::pubkey!("AKXpgmVhedTT49sekGbh6MXfx2kJKBcaFQLpc83NCp6N"),
        solana_pubkey::pubkey!("3MP45RTcFdLX3HpPD18asXCCxFjsgrZM9V6tEeQxFXd4"),
        solana_pubkey::pubkey!("88YC29WwpugANLcN7DYkuL3benW79WsdxkQ3KX8zER9s"),
        solana_pubkey::pubkey!("98j2yMfxrLzQ5YQJU1FMW3CTkoYgZStJirpm4HLAkA3o"),
        solana_pubkey::pubkey!("56AidDLicyQaLMqbMQeZ7itUmJyd7TAvehVDi8MXDYbx"),
        solana_pubkey::pubkey!("3yeCpRbBU5gzBaweVRqUXFcZUp7gSXBeknKcbGmjwmjJ"),
        solana_pubkey::pubkey!("5McU7LcsRw3RHyrjj98qHefx6ehy7MdjF1uKKi6Kp8hV"),
        solana_pubkey::pubkey!("3CYxk8uTXtANvbU1HW3NHNhKcBEfYesi2BdBFizy3f6s"),
        solana_pubkey::pubkey!("4bAowZuPRG8jzNwMceqjYjZnrGXQNR5ZCgQJM8s4hF4i"),
        solana_pubkey::pubkey!("ATWFW8xBGPe6xB8JnjyjZqG5FuEXnfyuhEocmqAqfeJG"),
        solana_pubkey::pubkey!("73Aa7yCd6AMPqsaLisfgrpjESVFtDrLnHEjfDsXBK3HV"),
    ]
    .into()
}

// Withdraw authority for autostaked accounts on mainnet-beta
pub fn withdraw_authority() -> Vec<Pubkey> {
    [
        solana_pubkey::pubkey!("8CUUMKYNGxdgYio5CLHRHyzMEhhVRMcqefgE6dLqnVRK"),
        solana_pubkey::pubkey!("3FFaheyqtyAXZSYxDzsr5CVKvJuvZD1WE1VEsBtDbRqB"),
        solana_pubkey::pubkey!("FdGYQdiRky8NZzN9wZtczTBcWLYYRXrJ3LMDhqDPn5rM"),
        solana_pubkey::pubkey!("4e6KwQpyzGQPfgVr5Jn3g5jLjbXB4pKPa2jRLohEb1QA"),
        solana_pubkey::pubkey!("FjiEiVKyMGzSLpqoB27QypukUfyWHrwzPcGNtopzZVdh"),
        solana_pubkey::pubkey!("DwbVjia1mYeSGoJipzhaf4L5hfer2DJ1Ys681VzQm5YY"),
        solana_pubkey::pubkey!("GeMGyvsTEsANVvcT5cme65Xq5MVU8fVVzMQ13KAZFNS2"),
        solana_pubkey::pubkey!("Bj3aQ2oFnZYfNR1njzRjmWizzuhvfcYLckh76cqsbuBM"),
        solana_pubkey::pubkey!("4ZJhPQAgUseCsWhKvJLTmmRRUV74fdoTpQLNfKoekbPY"),
        solana_pubkey::pubkey!("HXdYQ5gixrY2H6Y9gqsD8kPM2JQKSaRiohDQtLbZkRWE"),
        solana_pubkey::pubkey!("4sNBQyPbJCQyUimBueZkGWnLVqds4rWkm7eXyi9WskGU"),
        solana_pubkey::pubkey!("AZWdNvnZxJnbcT8ZzonpN19AZJadxPdUxSiCEDTJzu8L"),
        solana_pubkey::pubkey!("5kFTzLuM2VgFdb6x16smnY3JWoVdPxNZVFAqeVgjSTUP"),
        solana_pubkey::pubkey!("Gbz6wkNFus8SNEkWGNNENLv9NFwVvF1pWVDpaVKUWcMh"),
        solana_pubkey::pubkey!("AksPzoA9DKCipgdhHjhUzQJe4iEniCvBoEfvayuFA3BN"),
        solana_pubkey::pubkey!("DVhs8YHWrvhhGxoefDNY9KotqtEEnjnSAK8MYGL2Q7X"),
        solana_pubkey::pubkey!("2bvGnYAPSV8pa1H3vRYr5tPAXktP4DkFACHfAgqyyfhd"),
        solana_pubkey::pubkey!("2xP8YQ3sVmfNPtGM17tZi7Lr4vsPUiN6mHLx42roazG5"),
        solana_pubkey::pubkey!("3p9ZxnrSFTkXVrT3KnYg2tT6asnysDApEFB5DRkdeAhB"),
        solana_pubkey::pubkey!("wRVP5MYuqP8HJ1Q8RCJ5NzUraL3DxCKPGMKSBd5iQH1"),
        solana_pubkey::pubkey!("513qFSVgmAQFBDsnyFCM1MrVKBrWiDgb4nXrdGsqa7Z4"),
        solana_pubkey::pubkey!("619qLS85ieR4qh7MGNLLZyLefN7hMi2FDVb5cmX1nisb"),
        solana_pubkey::pubkey!("768NcPfBJpFBtjDAbYZLFkej6ca1W5jeArsW5MtdF8S9"),
        solana_pubkey::pubkey!("EWAjC8a9VPbALSM3D6tGsbRfgDV48kRuHZPu8qtYSNDv"),
        solana_pubkey::pubkey!("H8wurbnaaXsgtrjqkNH1HhncUPUhTLAmKUHkwMeyqmfN"),
        solana_pubkey::pubkey!("HBfi37TwD4kMa1WrAWwXp3ZaFbZQ1g3XxWwNZs8QsCpY"),
        solana_pubkey::pubkey!("6B7mXMM6BixHvDpPAPLSKweLyCXcbtprkfsw3HfMUSjZ"),
        solana_pubkey::pubkey!("5TuV9WpmESXNfTNqasXVehoXpQy65WUHBbzgKPXPLwWx"),
        solana_pubkey::pubkey!("FB6VmiYFnVGp1uKXA3WbNsqg9neGYVpwYYiB9q8bFRrQ"),
        solana_pubkey::pubkey!("7Anoa4ZRiq8qaaiEnhmdpXyTEmBjZASXGfnQqVDynNie"),
        solana_pubkey::pubkey!("6dXeE5hS8bQKeqZsc18ewCyqHimnhCBAvVJBusnRqa2F"),
        solana_pubkey::pubkey!("Ds69ZQPb3D3aVPXdN5REyzALBrzJLdruJZ1cwyfyoEEx"),
        solana_pubkey::pubkey!("5HA8QV7tp59iNpfjs8f84LGUGX4imaynMFSKWHUcTrMT"),
        solana_pubkey::pubkey!("EPfiDzgbdgXdyqqwbYFMqU6Qvfx2J9Zf2J9noXBYYbbx"),
        solana_pubkey::pubkey!("DEGyTHFXmYuyANRDYRoEcShBXLonWNdBRpNzbFZBuzhY"),
        solana_pubkey::pubkey!("9xz1vZSWgY6TFPLZgPM2WJjDM1KiPXcALnjYFHRTkYiK"),
        solana_pubkey::pubkey!("6n1mSmsdGFCkEyyHe4wtgEigbwhiwYRszrerMW9YRyYF"),
        solana_pubkey::pubkey!("8rp9vcJG1Uwk25g9XfQNHysfGBYcHizVEJvavqiffB6P"),
        solana_pubkey::pubkey!("BVsJuJbhXmxPB9XPZrYMXs1SfuZuo998u1jQ9emVp8YD"),
        solana_pubkey::pubkey!("Fas9FrnngyVjdMZ6r8p8SsfyFDtHLMmrw1dyBxRWWVTF"),
        solana_pubkey::pubkey!("6Lfk4e6yxXGcFNMHue5u8KQM9hWYKjbdMFZ2n7SHHd6L"),
        solana_pubkey::pubkey!("6WuU6DWoM7ZpgUVL5TR97Fgose7vLgrv4Qvg1Yhqv8Xw"),
        solana_pubkey::pubkey!("6mXiGJbQbX726ZhrMSmbLCF81E8rkSfo1pvBWyn1asG5"),
        solana_pubkey::pubkey!("2UdDthRdtZKMjEx9oQYnzuLdWy3sa4XKeaqihdXkXFza"),
        solana_pubkey::pubkey!("33XwSCq2Ft5tTCsGK7kTyMCJ4hqufDZT2REFAWXmsia8"),
        solana_pubkey::pubkey!("B5tRZWRCTCLANkY5zLnxEXAn8S2exJD9EkH3cacEYqmE"),
        solana_pubkey::pubkey!("CfCvwXZk1sLSqzuoFx1K7KG1puMofgwHA3tSpcArVysX"),
        solana_pubkey::pubkey!("C4fATCN93YL5iYS3c2MrHeDmkMD6waHuBXLqsri5q5AF"),
        solana_pubkey::pubkey!("HbJUXNHXLzpucDr9wgjuoqAwK6XNKEcqi5h29MkkuSBQ"),
        solana_pubkey::pubkey!("G8EBx1Qo5W8731nmaBSGYyvU96onpEP5adrVSjt4vnLF"),
        solana_pubkey::pubkey!("4dc5ty7TNod2EQfx4bPnDofPbQk5cELcn3it47qnLizL"),
        solana_pubkey::pubkey!("4VCET97mE8ixxAPRYabM7S7dvkiAFZ8ZzEoH6kbmfYus"),
        solana_pubkey::pubkey!("DwqBsPgtAHqQUmRN3WF5YTuLKhK7dqzrb9eRJ4UVRNLf"),
        solana_pubkey::pubkey!("26FqcAbTLvr6cVXk2ktPu9hpeMYWukY7uUwNCtTwu7Wb"),
        solana_pubkey::pubkey!("B22zqCsZL6PDsi4fftmcjj5k7hFjhH1BM8Dcz4fXfFXK"),
        solana_pubkey::pubkey!("9ahLw5LFpeTuBykK3A5aiFZ1CidTY7z4YxG2kmQdSGon"),
        solana_pubkey::pubkey!("8M731ZthMdeVoMFGRaPnUBzR7Ze6qY4zEcQ7WALEAn5i"),
        solana_pubkey::pubkey!("A3aaVsuX7BfNCUQbuepQSaLs9KNB7c6djG6u4UuJzwwP"),
        solana_pubkey::pubkey!("DeXE7LTUqsC3B9kSACfFdW5TPG1eT9n1Z6gVB9dJNxqz"),
        solana_pubkey::pubkey!("BtvSeDfZNfA3VEH6baFprfx6JcS6JX8jdt6NffCxoJGV"),
        solana_pubkey::pubkey!("8o8trB6XdG8o4qo29De2mcooY5E3pW2rvMMkeBwJHkUD"),
        solana_pubkey::pubkey!("CSSxN8vQYqSjg5GV6ZUGSM65dXqhytLzANLpMrBZjLKP"),
        solana_pubkey::pubkey!("5A2yoxdGdC3B3QC39pkXpjx2ZPH4WGgie9exHQnhNz4J"),
        solana_pubkey::pubkey!("DFaHYDcuLmxx3vrJGCgw8sPehzBNjvbvZTjz7PgvYLcb"),
        solana_pubkey::pubkey!("43Cwv866ActL11JqVQXoFPiytUYA3tFXUaNg1zPsarn9"),
        solana_pubkey::pubkey!("3wzw3L4wd3B7DHocbQxHosQXEfqEz5AnkQHKBisGKLzY"),
        solana_pubkey::pubkey!("EnX9MvycxMGKFpMPJHgNo6kD2proVaorKQcSaMmguMEZ"),
        solana_pubkey::pubkey!("GLz4zxemfHJy3BnK7PjZ4uUgJoHtycpT3JrmTDYduVpg"),
        solana_pubkey::pubkey!("362rDzQwChHJ9ToZgLdv8wzFHTTsxsxzFLTZPwKY3CYg"),
        solana_pubkey::pubkey!("3FKwgoQWCz4i43Pbo3jdfsjzkgrkjnk44rDWh7A9J9Ee"),
        solana_pubkey::pubkey!("Cj53e3n1WX9dAUNX5PZpsc9xG9e7hXGnuJkKLJkNaf6v"),
        solana_pubkey::pubkey!("Amn9KQZY5kJj932b49pxwqZuuKfNWewrmNQtdMrRfdvf"),
        solana_pubkey::pubkey!("6spvvQJzFCcw5fVVdF1zKxCeq8Gj2yRFGgFuG16fYSMo"),
        solana_pubkey::pubkey!("H5PutDz8EgAoznaX7Bc8qSB6sPxD2Xho145H6MH6AcbV"),
        solana_pubkey::pubkey!("HNCooAiRiVeqGK9VSjxsvRsfr4weTcx14XhkvM7zDMgx"),
        solana_pubkey::pubkey!("AUqLkd6bR54NtXY9Zt4J2zZ7CJbhVHuDR95YFk5qahaX"),
        solana_pubkey::pubkey!("H35YRuMGE6WDPBLu3fDYLxV7tEcL2yJ7C97jDkw6NrBf"),
        solana_pubkey::pubkey!("EhN8bv8LCXnNAVju9ERGsCccmdBNmNTAESAHXE8PyBLR"),
        solana_pubkey::pubkey!("Ht5roME7Fjzt3dcEyf7UxYpPPAAdyTrnJ6rgEwQrNAFg"),
        solana_pubkey::pubkey!("5zHRch6mowPiobRQHYXZ1siRcthw4dYkFPuaNDQkK3EE"),
        solana_pubkey::pubkey!("AZkdkkHzSEbaZXpr7xBmt71AHj9c8iWE2oWYELm3nns5"),
        solana_pubkey::pubkey!("FThHNjNQYi7FLvP4KjeYf3LXjcN2LhyYHmza35ucqtDT"),
        solana_pubkey::pubkey!("6vHscAGjgbTfi1ooRVWGjSiUhHcUnLBceJyuKjpKrCYy"),
        solana_pubkey::pubkey!("AnbszMCtjEVZTquG3FQ68x1f1WK5B1Z6E7gtN2J6vD5a"),
        solana_pubkey::pubkey!("5aUknZidNKmAPxLMHoUeWsEdmXX8sGKvj8s71pHTDC7H"),
        solana_pubkey::pubkey!("7Xgby7EgwoRkhbQSdNJL9sXwpo3jddUhqB4zv3WuJB4m"),
        solana_pubkey::pubkey!("43KdfhYK6LUXX4LmyxfZ1KLaQrfpRe8vNwopNS7wXsUC"),
        solana_pubkey::pubkey!("FG6UGgT4VpCQxjwQsVy6woHeemwb5jP5u9kR5ry3twcK"),
        solana_pubkey::pubkey!("BDvLyqobZFubd7ESEJde6KqjbNoChkdj5aEiBdCqcN5r"),
        solana_pubkey::pubkey!("BtF4msVoQYJPrQUMwNtEXw58LVjux18ddVhC4XjbsdwX"),
        solana_pubkey::pubkey!("GbPK6cgUKso2ZGNV6GRwbp3hkdUzTVjsybYQMGDBvMFa"),
        solana_pubkey::pubkey!("4bWjzNek2m6P3Q8rKkq11C47TaoAwpj3ykk5yDCYLWr1"),
        solana_pubkey::pubkey!("FPzPT7YV5BHSTjYjqycXbwBsD4E5WsrZNPcCgcHhPkUx"),
        solana_pubkey::pubkey!("EbitXxcwjC1dzrGJ7sa3Y6RBzSM3wxvrxMaBncNBKEf2"),
        solana_pubkey::pubkey!("HMmHHmCmC3oo4G4QCjKvNxctSBQzRFDp2WQHZoXyrW6Q"),
        solana_pubkey::pubkey!("145FLxWrDG9gKifyJKSnkKNAUPhG6hxUMEiZWw6XeSMW"),
        solana_pubkey::pubkey!("FnXgwJZT7RMhpYEJFCXGLHp63bhVLfVn5d4aSVQDe19n"),
        solana_pubkey::pubkey!("5tDptzMGG79DLp9XV4fdcmiRNPQGCkfBJeo8EVCSYKw7"),
        solana_pubkey::pubkey!("9oBNYSARUMuH2swnxzbxCi827YfUnYWyWSAkrHz93h5y"),
        solana_pubkey::pubkey!("DWfXRxmUXKdSTji4S1nXncviTmrhWQwxHAQCcvjRLeWm"),
        solana_pubkey::pubkey!("EHUv6Hc9XGAb57C8iNCUuoE9xnJnRTFr9LaP7zbu8LNB"),
        solana_pubkey::pubkey!("CTqnGTKHrcg8AVK5pSwVzpA3mmSxz7Js9y7rEqCbjryH"),
        solana_pubkey::pubkey!("7PzsZ3ApLcfQosSr2B1C9YV1GNNgriZVFyqzDpeHoAD"),
        solana_pubkey::pubkey!("AKXpgmVhedTT49sekGbh6MXfx2kJKBcaFQLpc83NCp6N"),
        solana_pubkey::pubkey!("3MP45RTcFdLX3HpPD18asXCCxFjsgrZM9V6tEeQxFXd4"),
        solana_pubkey::pubkey!("88YC29WwpugANLcN7DYkuL3benW79WsdxkQ3KX8zER9s"),
        solana_pubkey::pubkey!("98j2yMfxrLzQ5YQJU1FMW3CTkoYgZStJirpm4HLAkA3o"),
        solana_pubkey::pubkey!("56AidDLicyQaLMqbMQeZ7itUmJyd7TAvehVDi8MXDYbx"),
        solana_pubkey::pubkey!("3yeCpRbBU5gzBaweVRqUXFcZUp7gSXBeknKcbGmjwmjJ"),
        solana_pubkey::pubkey!("5McU7LcsRw3RHyrjj98qHefx6ehy7MdjF1uKKi6Kp8hV"),
        solana_pubkey::pubkey!("3CYxk8uTXtANvbU1HW3NHNhKcBEfYesi2BdBFizy3f6s"),
        solana_pubkey::pubkey!("4bAowZuPRG8jzNwMceqjYjZnrGXQNR5ZCgQJM8s4hF4i"),
        solana_pubkey::pubkey!("ATWFW8xBGPe6xB8JnjyjZqG5FuEXnfyuhEocmqAqfeJG"),
        solana_pubkey::pubkey!("73Aa7yCd6AMPqsaLisfgrpjESVFtDrLnHEjfDsXBK3HV"),
    ]
    .into()
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::genesis_utils::genesis_sysvar_and_builtin_program_lamports,
        solana_account::{Account, AccountSharedData},
        solana_cluster_type::ClusterType,
        solana_epoch_schedule::EpochSchedule,
        solana_genesis_config::GenesisConfig,
        solana_leader_schedule::SlotLeader,
        solana_stake_interface::state::{Authorized, Lockup, Meta},
        std::{collections::BTreeMap, sync::Arc},
    };

    fn new_from_parent(
        parent: Arc<Bank>,
        bank_forks: &std::sync::Arc<std::sync::RwLock<crate::bank_forks::BankForks>>,
    ) -> Arc<Bank> {
        let slot = parent.slot() + 1;
        Bank::new_from_parent_with_bank_forks(
            bank_forks.as_ref(),
            parent,
            SlotLeader::default(),
            slot,
        )
    }

    #[test]
    fn test_calculate_non_circulating_supply() {
        let mut accounts: BTreeMap<Pubkey, Account> = BTreeMap::new();
        let balance = 10;
        let num_genesis_accounts = 10;
        for _ in 0..num_genesis_accounts {
            accounts.insert(
                solana_pubkey::new_rand(),
                Account::new(balance, 0, &Pubkey::default()),
            );
        }
        let non_circulating_accounts = non_circulating_accounts();
        let num_non_circulating_accounts = non_circulating_accounts.len() as u64;
        for key in non_circulating_accounts.clone() {
            accounts.insert(key, Account::new(balance, 0, &Pubkey::default()));
        }

        let num_stake_accounts = 3;
        for _ in 0..num_stake_accounts {
            let pubkey = solana_pubkey::new_rand();
            let meta = Meta {
                authorized: Authorized::auto(&pubkey),
                lockup: Lockup {
                    epoch: 1,
                    ..Lockup::default()
                },
                ..Meta::default()
            };
            let stake_account = Account::new_data_with_space(
                balance,
                &StakeStateV2::Initialized(meta),
                StakeStateV2::size_of(),
                &stake::program::id(),
            )
            .unwrap();
            accounts.insert(pubkey, stake_account);
        }

        let slots_per_epoch = 32;
        let genesis_config = GenesisConfig {
            accounts,
            epoch_schedule: EpochSchedule::new(slots_per_epoch),
            cluster_type: ClusterType::MainnetBeta,
            ..GenesisConfig::default()
        };
        let (bank0, bank_forks) =
            Bank::new_for_tests(&genesis_config).wrap_with_bank_forks_for_tests();
        let mut bank = bank0;
        assert_eq!(
            bank.capitalization(),
            (num_genesis_accounts + num_non_circulating_accounts + num_stake_accounts) * balance
                + genesis_sysvar_and_builtin_program_lamports(),
        );

        let non_circulating_supply = calculate_non_circulating_supply(&bank).unwrap();
        assert_eq!(
            non_circulating_supply.lamports,
            (num_non_circulating_accounts + num_stake_accounts) * balance
        );
        assert_eq!(
            non_circulating_supply.accounts.len(),
            num_non_circulating_accounts as usize + num_stake_accounts as usize
        );

        bank = new_from_parent(bank, &bank_forks);
        let new_balance = 11;
        for key in non_circulating_accounts {
            bank.store_account(
                &key,
                &AccountSharedData::new(new_balance, 0, &Pubkey::default()),
            );
        }
        let non_circulating_supply = calculate_non_circulating_supply(&bank).unwrap();
        assert_eq!(
            non_circulating_supply.lamports,
            (num_non_circulating_accounts * new_balance) + (num_stake_accounts * balance)
        );
        assert_eq!(
            non_circulating_supply.accounts.len(),
            num_non_circulating_accounts as usize + num_stake_accounts as usize
        );

        // Advance bank an epoch, which should unlock stakes
        for _ in 0..slots_per_epoch {
            bank = new_from_parent(bank, &bank_forks);
        }
        assert_eq!(bank.epoch(), 1);
        let non_circulating_supply = calculate_non_circulating_supply(&bank).unwrap();
        assert_eq!(
            non_circulating_supply.lamports,
            num_non_circulating_accounts * new_balance
        );
        assert_eq!(
            non_circulating_supply.accounts.len(),
            num_non_circulating_accounts as usize
        );
    }
}
