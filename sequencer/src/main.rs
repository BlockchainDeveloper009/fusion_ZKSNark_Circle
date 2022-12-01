use std::{
    collections::HashMap,
    net::SocketAddr,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use clap::{Parser, Subcommand};
use ethers::{
    abi::Address,
    core::k256::SecretKey,
    providers::{Http, Provider},
    signers::{LocalWallet, Signer},
    types,
    utils::keccak256,
};
use hyper::Method;
use jsonrpsee::{
    server::{AllowHosts, ServerBuilder, ServerHandle},
    RpcModule,
};
use serde::{Deserialize, Serialize};
use tokio::{task, time::interval};
use tower_http::cors::{Any, CorsLayer};

mod node;
use node::Node;

use l2_bindings::l2;

#[derive(Debug, Serialize, Deserialize)]
struct Tx {
    from: Address,
    to: Address,
    nonce: types::U256,
    value: types::U256,
}

impl From<CLITx> for Tx {
    fn from(tx: CLITx) -> Self {
        Self {
            from: tx.from,
            to: tx.to,
            nonce: tx.nonce,
            value: tx.value,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct SignedTx {
    tx: Tx,
    signature: String,
}

impl From<CLITx> for SignedTx {
    fn from(tx: CLITx) -> Self {
        Self {
            tx: Tx {
                from: tx.from,
                to: tx.to,
                nonce: tx.nonce,
                value: tx.value,
            },
            signature: tx.signature.unwrap(),
        }
    }
}

impl From<SignedTx> for l2::Tx {
    fn from(tx: SignedTx) -> Self {
        Self {
            from: tx.tx.from,
            to: tx.tx.to,
            amt: tx.tx.value,
            nonce: tx.tx.nonce,
            signature: tx.signature.parse().unwrap(),
        }
    }
}

type Db = Arc<Mutex<Vec<SignedTx>>>;

const DB_PATH: &str = "./db";
const SOCKET_ADDRESS: &str = "127.0.0.1:38171";
const SERVER_ADDRESS: &str = "http://localhost:38171";

#[derive(Debug, Parser)]
#[clap(name = "trollup sequencer", version = env!("CARGO_PKG_VERSION"))]
struct Opts {
    #[clap(subcommand)]
    pub sub: Option<Subcommands>,
}

#[derive(Debug, Subcommand)]
pub enum Subcommands {
    #[clap(about = "Sign a trollup transaction.")]
    Sign(CLITx),
    #[clap(about = "Send trollup transaction, potentially sign it before.")]
    Send(CLITx),
}

#[derive(Debug, Clone, Parser, Default)]
pub struct CLITx {
    #[clap(
        long,
        short = 'p',
        value_name = "PRIVATE_KEY",
        help = "The private key that signs the message",
        default_value = "0x0000000000000000000000000000000000000000000000000000000000000000"
    )]
    pub private_key: ethers::types::H256,
    #[clap(
        long,
        short = 'f',
        value_name = "FROM_ADDRESS",
        help = "The address of the from address.",
        default_value = "0x0000000000000000000000000000000000000000"
    )]
    pub from: ethers::types::Address,
    #[clap(
        long,
        short = 't',
        value_name = "DEST_ADDRESS",
        help = "The address of the destination address.",
        default_value = "0x0000000000000000000000000000000000000000"
    )]
    pub to: ethers::types::Address,
    #[clap(
        long,
        short = 'v',
        value_name = "VALUE",
        help = "The value of the transaction.",
        default_value = "0"
    )]
    pub value: ethers::types::U256,
    #[clap(
        long,
        short = 'n',
        value_name = "NONCE",
        help = "The nonce of the transaction.",
        default_value = "0"
    )]
    pub nonce: ethers::types::U256,
    #[clap(
        long,
        short = 's',
        value_name = "SIGNATURE",
        help = "The signed transaction."
    )]
    pub signature: Option<String>,
}

async fn run_node() -> anyhow::Result<()> {
    let db_path = Path::new(DB_PATH);
    let db = init_db(db_path);
    let rpc = init_rpc(db.clone()).await.unwrap();

    let private_key = std::env::var("ETH_PRIVATE_KEY")?;
    let http_endpoint = std::env::var("ETH_RPC_URL")?;

    task::spawn(async move {
        let l1_contract = init_l1(private_key, http_endpoint).await.unwrap();
        let mut interval = interval(Duration::from_millis(1000 * 5));

        let addr0: types::Address = "0x318A2475f1ba1A1AC4562D1541512d3649eE1131"
            .parse()
            .unwrap();
        let addr1: types::Address = "0x419978a8729ed2c3b1048b5Bba49f8599eD8F7C1"
            .parse()
            .unwrap();

        loop {
            interval.tick().await;

            let current_root = l1_contract.root().call().await.unwrap();
            println!("Current root is {}", types::H256::from(current_root));

            let state = l1_contract.current_state().call().await.unwrap();
            let state = HashMap::<types::Address, types::U256>::from([
                (addr0, state[0]),
                (addr1, state[1]),
            ]);
            println!("Current L1 state is {:?}", state);

            let txs: Vec<_> = db
                .lock()
                .unwrap()
                .drain(..)
                .filter(|tx| validate_tx(&state, tx).is_ok())
                .collect();

            let state = txs.iter().fold(state, apply_tx);
            println!("Computed L2 state is {:?}", state);
            l1_contract
                .submit_block(
                    txs.into_iter().map(|tx| tx.into()).collect(),
                    compute_root(&state).into(),
                )
                .send()
                .await
                .unwrap();
        }
    });

    tokio::spawn(rpc.stopped());

    println!("Run the following snippet in the developer console in any Website.");
    println!(
        r#"
        fetch("http://{}", {{
            method: 'POST',
            mode: 'cors',
            headers: {{ 'Content-Type': 'application/json' }},
            body: JSON.stringify({{
                jsonrpc: '2.0',
                method: 'submit_transaction',
                params: {{
                    from: '0x0000000000000000000000000000000000000000',
                    to: '0x0000000000000000000000000000000000000000',
                    amount: 42
                }},
                id: 1
            }})
        }}).then(res => {{
            console.log("Response:", res);
            return res.text()
        }}).then(body => {{
            console.log("Response Body:", body)
        }});
    "#,
        SOCKET_ADDRESS
    );

    futures::future::pending().await
}

fn validate_tx(state: &HashMap<types::Address, types::U256>, tx: &SignedTx) -> anyhow::Result<()> {
    match state.get(&tx.tx.from) {
        Some(entry) if *entry >= tx.tx.value => Ok(()),
        _ => Err(anyhow::anyhow!("Insufficient balance")),
    }
}

fn apply_tx(
    mut state: HashMap<types::Address, types::U256>,
    tx: &SignedTx,
) -> HashMap<types::Address, types::U256> {
    match state.get_mut(&tx.tx.from) {
        Some(entry) if *entry >= tx.tx.value => {
            *entry -= tx.tx.value;
        }
        _ => panic!(),
    };
    *state.entry(tx.tx.to).or_insert_with(|| 0.into()) += tx.tx.value;
    state
}

fn compute_root(state: &HashMap<types::Address, types::U256>) -> types::H256 {
    let addr0: types::Address = "0x318A2475f1ba1A1AC4562D1541512d3649eE1131"
        .parse()
        .unwrap();
    let addr1: types::Address = "0x419978a8729ed2c3b1048b5Bba49f8599eD8F7C1"
        .parse()
        .unwrap();

    let mut addr0_bytes = vec![0; 32];
    state[&addr0].to_big_endian(&mut addr0_bytes);

    let mut addr1_bytes = vec![0; 32];
    state[&addr1].to_big_endian(&mut addr1_bytes);

    keccak256([addr0_bytes, addr1_bytes].concat()).into()
}

fn hash_tx(sig_args: &Tx) -> ethers::types::TxHash {
    let mut value_bytes = vec![0; 32];
    sig_args.value.to_big_endian(&mut value_bytes);

    let mut nonce_bytes = vec![0; 32];
    sig_args.nonce.to_big_endian(&mut nonce_bytes);

    let msg = [
        sig_args.from.as_fixed_bytes().to_vec(),
        sig_args.to.as_fixed_bytes().to_vec(),
        value_bytes,
        nonce_bytes,
    ]
    .concat();

    types::TxHash::from(keccak256(msg))
}

async fn sign(sig_args: CLITx) -> anyhow::Result<types::Signature> {
    let wallet: LocalWallet = SecretKey::from_be_bytes(sig_args.private_key.as_bytes())
        .expect("invalid private key")
        .into();

    let hash = hash_tx(&sig_args.into()).as_fixed_bytes().to_vec();
    let signature = wallet.sign_message(hash.clone()).await?;

    Ok(signature)
}

fn verify_tx_signature(signed_tx: &SignedTx) -> anyhow::Result<()> {
    let hash = hash_tx(&signed_tx.tx).as_fixed_bytes().to_vec();
    let decoded = signed_tx.signature.parse::<types::Signature>()?;
    decoded.verify(hash, signed_tx.tx.from)?;

    Ok(())
}

async fn send(send_args: CLITx) -> anyhow::Result<()> {
    let signed: SignedTx = if send_args.signature.is_some() {
        send_args.clone().into()
    } else {
        SignedTx {
            tx: send_args.clone().into(),
            signature: sign(send_args).await?.to_string(),
        }
    };

    verify_tx_signature(&signed)?;

    let provider =
        Provider::<Http>::try_from(SERVER_ADDRESS)?.interval(Duration::from_millis(10u64));
    let client = Arc::new(provider);
    let tx_receipt = client.request("submit_transaction", signed).await?;
    println!("{:?}", tx_receipt);

    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let opts = Opts::parse();
    match opts.sub {
        Some(Subcommands::Sign(sig_args)) => {
            let signature = sign(sig_args).await?;
            println!("{}", signature);
            Ok(())
        }
        Some(Subcommands::Send(send_args)) => send(send_args).await,
        _ => run_node().await,
    }
}

fn init_db(path: &Path) -> Db {
    Arc::new(Mutex::new(vec![]))
}

async fn init_l1(
    private_key: String,
    http_endpoint: String,
) -> anyhow::Result<l2::L2<ethers::middleware::SignerMiddleware<Provider<Http>, LocalWallet>>> {
    let node = Arc::new(Node::new_with_private_key(private_key, http_endpoint).await?);

    let l2_address: types::Address = std::env::var("TROLLUP_L1_CONTRACT")?.parse()?;
    let l2_contract = l2::L2::new(l2_address, node.http_client.clone());

    Ok(l2_contract)
}

async fn init_rpc(db: Db) -> anyhow::Result<ServerHandle> {
    let cors = CorsLayer::new()
        // Allow `POST` when accessing the resource
        .allow_methods([Method::POST])
        // Allow requests from any origin
        .allow_origin(Any)
        .allow_headers([hyper::header::CONTENT_TYPE]);
    let middleware = tower::ServiceBuilder::new().layer(cors);

    let server = ServerBuilder::default()
        .set_host_filtering(AllowHosts::Any)
        .set_middleware(middleware)
        .build(SOCKET_ADDRESS.parse::<SocketAddr>()?)
        .await?;

    println!("{}", server.local_addr().unwrap());

    let mut module = RpcModule::new(());
    module.register_method("submit_transaction", move |params, _| {
        println!("received transaction! {:?}", params);
        let tx: SignedTx = params.parse()?;

        verify_tx_signature(&tx)?;

        let mut db = db.lock().unwrap();
        db.push(tx);
        Ok(())
    })?;

    let handle = server.start(module)?;

    Ok(handle)
}