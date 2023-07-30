pub mod tests {
    use ethers::{
        abi::{Bytes, Contract, Function, Param, ParamType, StateMutability, Token},
        prelude::{k256::ecdsa::SigningKey, ContractFactory, SignerMiddleware},
        providers::{Http, Middleware, Provider},
        signers::{Signer, Wallet},
        solc::{CompilerInput, Solc},
        types::{transaction::eip2718::TypedTransaction, TransactionRequest, U256},
    };
    use halo2_solidity_verifier::fix_verifier_sol;
    use snark_verifier::{
        loader::evm::EvmLoader,
        pcs::kzg::{Gwc19, KzgAs, KzgDecidingKey},
        system::halo2::{transcript::evm::EvmTranscript, Config},
        verifier::{plonk::PlonkProof, SnarkVerifier},
    };
    use std::{
        error::Error,
        fs::File,
        io::Write,
        marker::PhantomData,
        path::PathBuf,
        process::{Child, Command},
        rc::Rc,
        sync::Arc,
    };

    use halo2_proofs::{
        arithmetic::Field,
        circuit::{AssignedCell, Chip, Layouter, Region, SimpleFloorPlanner, Value},
        halo2curves::{
            bn256::{Bn256, Fq, Fr, G1Affine},
            ff::PrimeField,
        },
        plonk::{
            create_proof, keygen_pk, keygen_vk, Advice, Circuit, Column, ConstraintSystem, Fixed,
            Instance, Selector,
        },
        poly::{
            commitment::ParamsProver,
            kzg::{
                commitment::{KZGCommitmentScheme, ParamsKZG},
                multiopen::ProverGWC,
            },
            Rotation,
        },
        transcript::TranscriptWriterBuffer,
    };
    use log::info;
    use rand::rngs::OsRng;

    type PlonkVerifier = snark_verifier::verifier::plonk::PlonkVerifier<KzgAs<Bn256, Gwc19>>;
    pub type EthersClient = Arc<SignerMiddleware<Provider<Http>, Wallet<SigningKey>>>;

    fn start_anvil() -> Child {
        let child = Command::new("anvil")
            .args(["-p", "3030"])
            // .stdout(Stdio::piped())
            .spawn()
            .expect("failed to start anvil process");

        std::thread::sleep(std::time::Duration::from_secs(3));
        child
    }

    trait NumericInstructions<F: Field>: Chip<F> {
        /// Variable representing a number.
        type Num;

        /// Loads a number into the circuit as a private input.
        fn load_private(
            &self,
            layouter: impl Layouter<F>,
            a: Value<F>,
        ) -> Result<Self::Num, halo2_proofs::plonk::Error>;

        /// Loads a number into the circuit as a fixed constant.
        fn load_constant(
            &self,
            layouter: impl Layouter<F>,
            constant: F,
        ) -> Result<Self::Num, halo2_proofs::plonk::Error>;

        /// Returns `c = a * b`.
        fn mul(
            &self,
            layouter: impl Layouter<F>,
            a: Self::Num,
            b: Self::Num,
        ) -> Result<Self::Num, halo2_proofs::plonk::Error>;

        /// Exposes a number as a public input to the circuit.
        fn expose_public(
            &self,
            layouter: impl Layouter<F>,
            num: Self::Num,
            row: usize,
        ) -> Result<(), halo2_proofs::plonk::Error>;
    }

    /// The chip that will implement our instructions! Chips store their own
    /// config, as well as type markers if necessary.
    struct FieldChip {
        config: FieldConfig,
        _marker: PhantomData<Fr>,
    }

    /// Chip state is stored in a config struct. This is generated by the chip
    /// during configuration, and then stored inside the chip.
    #[derive(Clone, Debug)]
    struct FieldConfig {
        /// For this chip, we will use two advice columns to implement our instructions.
        /// These are also the columns through which we communicate with other parts of
        /// the circuit.
        advice: [Column<Advice>; 2],

        /// This is the public input (instance) column.
        instance: Column<Instance>,

        // We need a selector to enable the multiplication gate, so that we aren't placing
        // any constraints on cells where `NumericInstructions::mul` is not being used.
        // This is important when building larger circuits, where columns are used by
        // multiple sets of instructions.
        s_mul: Selector,
    }

    impl Chip<Fr> for FieldChip {
        type Config = FieldConfig;
        type Loaded = ();

        fn config(&self) -> &Self::Config {
            &self.config
        }

        fn loaded(&self) -> &Self::Loaded {
            &()
        }
    }

    impl FieldChip {
        fn construct(config: <Self as Chip<Fr>>::Config) -> Self {
            Self {
                config,
                _marker: PhantomData,
            }
        }

        fn configure(
            meta: &mut ConstraintSystem<Fr>,
            advice: [Column<Advice>; 2],
            instance: Column<Instance>,
            constant: Column<Fixed>,
        ) -> <Self as Chip<Fr>>::Config {
            meta.enable_equality(instance);
            meta.enable_constant(constant);
            for column in &advice {
                meta.enable_equality(*column);
            }
            let s_mul = meta.selector();

            // Define our multiplication gate!
            meta.create_gate("mul", |meta| {
                // To implement multiplication, we need three advice cells and a selector
                // cell. We arrange them like so:
                //
                // | a0  | a1  | s_mul |
                // |-----|-----|-------|
                // | lhs | rhs | s_mul |
                // | out |     |       |
                //
                // Gates may refer to any relative offsets we want, but each distinct
                // offset adds a cost to the proof. The most common offsets are 0 (the
                // current row), 1 (the next row), and -1 (the previous row), for which
                // `Rotation` has specific constructors.
                let lhs = meta.query_advice(advice[0], Rotation::cur());
                let rhs = meta.query_advice(advice[1], Rotation::cur());
                let out = meta.query_advice(advice[0], Rotation::next());
                let s_mul = meta.query_selector(s_mul);

                // Finally, we return the polynomial expressions that constrain this gate.
                // For our multiplication gate, we only need a single polynomial constraint.
                //
                // The polynomial expressions returned from `create_gate` will be
                // constrained by the proving system to equal zero. Our expression
                // has the following properties:
                // - When s_mul = 0, any value is allowed in lhs, rhs, and out.
                // - When s_mul != 0, this constrains lhs * rhs = out.
                vec![s_mul * (lhs * rhs - out)]
            });

            FieldConfig {
                advice,
                instance,
                s_mul,
            }
        }
    }

    /// A variable representing a number.
    #[derive(Clone)]
    struct Number<F: Field>(AssignedCell<F, F>);

    impl NumericInstructions<Fr> for FieldChip {
        type Num = Number<Fr>;

        fn load_private(
            &self,
            mut layouter: impl Layouter<Fr>,
            value: Value<Fr>,
        ) -> Result<Self::Num, halo2_proofs::plonk::Error> {
            let config = self.config();

            layouter.assign_region(
                || "load private",
                |mut region| {
                    region
                        .assign_advice(|| "private input", config.advice[0], 0, || value)
                        .map(Number)
                },
            )
        }

        fn load_constant(
            &self,
            mut layouter: impl Layouter<Fr>,
            constant: Fr,
        ) -> Result<Self::Num, halo2_proofs::plonk::Error> {
            let config = self.config();

            layouter.assign_region(
                || "load constant",
                |mut region| {
                    region
                        .assign_advice_from_constant(
                            || "constant value",
                            config.advice[0],
                            0,
                            constant,
                        )
                        .map(Number)
                },
            )
        }

        fn mul(
            &self,
            mut layouter: impl Layouter<Fr>,
            a: Self::Num,
            b: Self::Num,
        ) -> Result<Self::Num, halo2_proofs::plonk::Error> {
            let config = self.config();

            layouter.assign_region(
                || "mul",
                |mut region: Region<'_, Fr>| {
                    // We only want to use a single multiplication gate in this region,
                    // so we enable it at region offset 0; this means it will constrain
                    // cells at offsets 0 and 1.
                    config.s_mul.enable(&mut region, 0)?;

                    // The inputs we've been given could be located anywhere in the circuit,
                    // but we can only rely on relative offsets inside this region. So we
                    // assign new cells inside the region and constrain them to have the
                    // same values as the inputs.
                    a.0.copy_advice(|| "lhs", &mut region, config.advice[0], 0)?;
                    b.0.copy_advice(|| "rhs", &mut region, config.advice[1], 0)?;

                    // Now we can assign the multiplication result, which is to be assigned
                    // into the output position.
                    let value = a.0.value().copied() * b.0.value();

                    // Finally, we do the assignment to the output, returning a
                    // variable to be used in another part of the circuit.
                    region
                        .assign_advice(|| "lhs * rhs", config.advice[0], 1, || value)
                        .map(Number)
                },
            )
        }

        fn expose_public(
            &self,
            mut layouter: impl Layouter<Fr>,
            num: Self::Num,
            row: usize,
        ) -> Result<(), halo2_proofs::plonk::Error> {
            let config = self.config();

            layouter.constrain_instance(num.0.cell(), config.instance, row)
        }
    }

    /// The full circuit implementation.
    ///
    /// In this struct we store the private input variables. We use `Option<F>` because
    /// they won't have any value during key generation. During proving, if any of these
    /// were `None` we would get an error.
    #[derive(Default)]
    struct MyCircuit {
        constant: Fr,
        a: Value<Fr>,
        b: Value<Fr>,
    }

    impl Circuit<Fr> for MyCircuit {
        // Since we are using a single chip for everything, we can just reuse its config.
        type Config = FieldConfig;
        type FloorPlanner = SimpleFloorPlanner;

        fn without_witnesses(&self) -> Self {
            Self::default()
        }

        fn configure(meta: &mut ConstraintSystem<Fr>) -> Self::Config {
            // We create the two advice columns that FieldChip uses for I/O.
            let advice = [meta.advice_column(), meta.advice_column()];

            // We also need an instance column to store public inputs.
            let instance = meta.instance_column();

            // Create a fixed column to load constants.
            let constant = meta.fixed_column();

            FieldChip::configure(meta, advice, instance, constant)
        }

        fn synthesize(
            &self,
            config: Self::Config,
            mut layouter: impl Layouter<Fr>,
        ) -> Result<(), halo2_proofs::plonk::Error> {
            let field_chip = FieldChip::construct(config);

            // Load our private values into the circuit.
            let a = field_chip.load_private(layouter.namespace(|| "load a"), self.a)?;
            let b = field_chip.load_private(layouter.namespace(|| "load b"), self.b)?;

            // Load the constant factor into the circuit.
            let constant =
                field_chip.load_constant(layouter.namespace(|| "load constant"), self.constant)?;

            // We only have access to plain multiplication.
            // We could implement our circuit as:
            //     asq  = a*a
            //     bsq  = b*b
            //     absq = asq*bsq
            //     c    = constant*asq*bsq
            //
            // but it's more efficient to implement it as:
            //     ab   = a*b
            //     absq = ab^2
            //     c    = constant*absq
            let ab = field_chip.mul(layouter.namespace(|| "a * b"), a, b)?;
            let absq = field_chip.mul(layouter.namespace(|| "ab * ab"), ab.clone(), ab)?;
            let c = field_chip.mul(layouter.namespace(|| "constant * absq"), constant, absq)?;

            // Expose the result as a public input to the circuit.
            field_chip.expose_public(layouter.namespace(|| "expose c"), c, 0)
        }
    }

    /// Return an instance of Anvil and a client for the given RPC URL. If none is provided, a local client is used.
    pub async fn setup_eth_backend(
        rpc_url: Option<&str>,
    ) -> Result<(ethers::utils::AnvilInstance, EthersClient), Box<dyn Error>> {
        // Launch anvil

        use ethers::{signers::LocalWallet, utils::Anvil};
        let anvil = Anvil::new().spawn();

        // Instantiate the wallet
        let wallet: LocalWallet = anvil.keys()[0].clone().into();

        let endpoint = if let Some(rpc_url) = rpc_url {
            rpc_url.to_string()
        } else {
            anvil.endpoint()
        };

        // Connect to the network
        let provider =
            Provider::<Http>::try_from(endpoint)?.interval(std::time::Duration::from_millis(10u64));

        let chain_id = provider.get_chainid().await?;
        info!("using chain {}", chain_id);

        // Instantiate the client with the wallet
        let client = Arc::new(SignerMiddleware::new(
            provider,
            wallet.with_chain_id(anvil.chain_id()),
        ));

        Ok((anvil, client))
    }

    fn get_contract_artifacts(
        sol_code_path: PathBuf,
        contract_name: &str,
        runs: Option<usize>,
    ) -> Result<(Contract, Bytes, Bytes), Box<dyn Error>> {
        assert!(sol_code_path.exists());
        // Create the compiler input, enabling the optimizer and setting the optimzer runs.
        let input: CompilerInput = if let Some(r) = runs {
            let mut i = CompilerInput::new(sol_code_path)?[0].clone().optimizer(r);
            i.settings.optimizer.enable();
            i
        } else {
            CompilerInput::new(sol_code_path)?[0].clone()
        };
        let compiled = Solc::default().compile(&input).unwrap();
        let (abi, bytecode, runtime_bytecode) = compiled
            .find(contract_name)
            .expect("could not find contract")
            .into_parts_or_default();
        Ok((abi, bytecode.to_vec(), runtime_bytecode.to_vec()))
    }

    /// Generates the contract factory for a solidity verifier, optionally compiling the code with optimizer runs set on the Solc compiler.
    fn get_sol_contract_factory<M: 'static + Middleware>(
        abi: Contract,
        bytecode: Bytes,
        runtime_bytecode: Bytes,
        client: Arc<M>,
    ) -> Result<ContractFactory<M>, Box<dyn Error>> {
        const MAX_RUNTIME_BYTECODE_SIZE: usize = 24577;
        let size = runtime_bytecode.len();
        println!("runtime bytecode size: {:#?}", size);
        if size > MAX_RUNTIME_BYTECODE_SIZE {
            // `_runtime_bytecode` exceeds the limit
            panic!(
                "Solidity runtime bytecode size is: {:#?},
            which exceeds 24577 bytes limit.",
                size
            );
        }
        Ok(ContractFactory::new(abi, bytecode.into(), client))
    }

    #[tokio::test]
    pub async fn test_verifier_can_verify() {
        // The number of rows in our circuit cannot exceed 2^k. Since our example
        // circuit is very small, we can pick a very small value here.
        let k = 4;
        let srs = ParamsKZG::<Bn256>::new(k);

        // Prepare the private and public inputs to the circuit!
        let constant = Fr::from(7);
        let a = Fr::from(2);
        let b = Fr::from(3);
        let c = constant * a.square() * b.square();

        // Instantiate the circuit with the private inputs.
        let circuit = MyCircuit {
            constant,
            a: Value::known(a),
            b: Value::known(b),
        };

        let vk = keygen_vk(&srs, &circuit).unwrap();
        let pk = keygen_pk(&srs, vk.clone(), &circuit).unwrap();

        let pi_inner: &[&[&[Fr]]] = &[&[&[c]]];

        let mut transcript = EvmTranscript::<G1Affine, _, _, _>::init(vec![]);
        let mut rng = OsRng;

        create_proof::<KZGCommitmentScheme<_>, ProverGWC<_>, _, _, _, _>(
            &srs,
            &pk,
            &[circuit],
            pi_inner,
            &mut rng,
            &mut transcript,
        )
        .unwrap();
        let proof = transcript.finalize();

        let protocol = snark_verifier::system::halo2::compile(
            &srs,
            &vk,
            Config::kzg().with_num_instance(vec![1]),
        );

        // get yul code
        let loader = EvmLoader::new::<Fq, Fr>();
        let deciding_key: KzgDecidingKey<Bn256> = (srs.get_g()[0], srs.g2(), srs.s_g2()).into();
        let protocol = protocol.loaded(&loader);
        let mut verifier_transcript = EvmTranscript::<_, Rc<EvmLoader>, _, _>::new(&loader);
        let instances = verifier_transcript.load_instances(vec![1]);
        let plonk: PlonkProof<G1Affine, Rc<EvmLoader>, KzgAs<Bn256, Gwc19>> =
            PlonkVerifier::read_proof(
                &deciding_key,
                &protocol,
                &instances,
                &mut verifier_transcript,
            )
            .unwrap();
        PlonkVerifier::verify(&deciding_key, &protocol, &instances, &plonk).unwrap();
        let yul_code = &loader.yul_code();

        let yul_code_path = PathBuf::from("test.yul");

        let mut f = File::create(yul_code_path.clone()).unwrap();
        let _ = f.write(yul_code.as_bytes());

        // now get sol verifier
        let sol_contract = fix_verifier_sol(yul_code_path.clone(), 1, None, None).unwrap();

        let sol_code_path = PathBuf::from("test.sol");
        let mut f = File::create(sol_code_path.clone()).unwrap();
        let _ = f.write(sol_contract.as_bytes());

        // now deploy
        let mut anvil_child = start_anvil();
        let rpc_url = "http://localhost:3030";
        let (_, client) = setup_eth_backend(Some(rpc_url)).await.unwrap();
        let (abi, bytecode, runtime_bytecode) =
            get_contract_artifacts(sol_code_path, "Verifier", None).unwrap();
        let factory =
            get_sol_contract_factory(abi, bytecode, runtime_bytecode, client.clone()).unwrap();
        let contract = factory.deploy(()).unwrap().send().await.unwrap();
        let addr = contract.address();
        println!("Contract deployed at: {:#?}", addr);

        //
        let mut public_inputs: Vec<U256> = vec![];

        for val in pi_inner[0][0].iter() {
            let bytes = val.to_repr();
            let u = U256::from_little_endian(bytes.as_slice());
            public_inputs.push(u);
        }

        #[allow(deprecated)]
        let func = Function {
            name: "verify".to_owned(),
            inputs: vec![
                Param {
                    name: "pubInputs".to_owned(),
                    kind: ParamType::FixedArray(
                        Box::new(ParamType::Uint(256)),
                        public_inputs.len(),
                    ),
                    internal_type: None,
                },
                Param {
                    name: "proof".to_owned(),
                    kind: ParamType::Bytes,
                    internal_type: None,
                },
            ],
            outputs: vec![Param {
                name: "success".to_owned(),
                kind: ParamType::Bool,
                internal_type: None,
            }],
            constant: None,
            state_mutability: StateMutability::View,
        };

        let encoded = func
            .encode_input(&[
                Token::FixedArray(public_inputs.clone().into_iter().map(Token::Uint).collect()),
                Token::Bytes(proof.clone()),
            ])
            .unwrap();

        let tx: TypedTransaction = TransactionRequest::default()
            .to(addr)
            .from(client.address())
            .data(encoded)
            .into();

        let result = client.call(&tx, None).await;
        assert!(result.is_ok());

        let result = result.unwrap();
        let result = result.to_vec().last().unwrap() == &1u8;
        assert!(result);

        println!("Success: {:#?}", result);

        // now test with wrong instances
        let mut public_inputs = public_inputs.clone();
        public_inputs[0] = U256::from(0);

        let encoded = func
            .encode_input(&[
                Token::FixedArray(public_inputs.into_iter().map(Token::Uint).collect()),
                Token::Bytes(proof),
            ])
            .unwrap();
        let tx: TypedTransaction = TransactionRequest::default()
            .to(addr)
            .from(client.address())
            .data(encoded)
            .into();
        let result = client.call(&tx, None).await;
        // assert executed ok
        assert!(result.is_ok());

        let result = result.unwrap();
        let result = result.to_vec().last().unwrap() == &1u8;
        assert!(!result);

        println!("Bad Instance Success: {:#?}", result);

        anvil_child.kill().unwrap();
    }
}
