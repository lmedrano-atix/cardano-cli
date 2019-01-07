use cardano::{
    address::ExtendedAddr,
    config::ProtocolMagic,
    tx::{TxInWitness, TxoPointer},
    util::hex,
};
use std::{error, fmt, path::PathBuf};
use storage_units::{
    append,
    utils::{
        lock::{self, Lock},
        serialize,
    },
};

use super::operation::ParsingOperationError;
use super::transaction;
use super::{config, Change, Input, Operation, Output, StagingId, Transaction};

pub struct StagingTransaction {
    /// the unique Staging ID associated to this staging
    /// transaction
    pub id: StagingId,

    /// blockchain's identifier
    pub protocol_magic: ProtocolMagic,

    /// keep the vector of operations associated to this transaction
    pub operations: Vec<Operation>,

    /// the transaction under construction
    pub transaction: Transaction,

    /// keep a lock to the staging transaction file for as long as this object
    /// exist. This will prevent having code that opens the same staging
    /// transaction multiple time.
    pub writer: append::Writer,
}

const MAGIC_TRANSACTION_V1: &'static [u8] = b"TRANSACTION_V1";

impl StagingTransaction {
    fn new_with(
        root_dir: PathBuf,
        protocol_magic: ProtocolMagic,
        id: StagingId,
    ) -> append::Result<Self> {
        let path = config::transaction_file(root_dir, id);

        if path.is_file() {
            // the staging transaction already exist
            assert!(!path.is_file(), "Staging transaction already exists");
        }

        let lock = Lock::lock(path)?;
        let mut w = append::Writer::open(lock)?;
        w.append_bytes(MAGIC_TRANSACTION_V1)?;
        {
            let mut bytes = Vec::with_capacity(4);
            serialize::utils::write_u32(&mut bytes, *protocol_magic)?;
            w.append_bytes(&bytes)?;
        }
        Ok(StagingTransaction {
            id: id,
            protocol_magic: protocol_magic,
            operations: Vec::new(),
            transaction: Transaction::new(),
            writer: w,
        })
    }

    /// create a new staging transaction.
    ///
    /// The `root_dir` is necessary as it will create the file (and the necessary
    /// directories) where the transactions will be stored
    ///
    pub fn new(root_dir: PathBuf, protocol_magic: ProtocolMagic) -> append::Result<Self> {
        let id = StagingId::generate();
        Self::new_with(root_dir, protocol_magic, id)
    }

    /// destroy the staging transaction from the file system
    pub fn destroy(self) -> append::Result<()> {
        // close the writer
        let lock = self.writer.close();
        ::std::fs::remove_file(&lock)?;
        Ok(())
    }

    /// prepare the `StagingTransaction` to be exported into a human
    /// or a smaller format.
    ///
    /// Note: the Export does not include the operation history. Only the
    /// necessary details.
    pub fn export(&self) -> Export {
        Export::from(self)
    }

    /// reconstruct a staging transaction from an `Export`.
    ///
    pub fn import(root_dir: PathBuf, export: Export) -> Result<Self, StagingUpdateError> {
        debug!("transaction file's magic `{}'", export.magic);
        let mut st = Self::new_with(root_dir, export.protocol_magic, export.staging_id)?;

        for input in export.transaction.inputs {
            st.add_input(input)?;
        }
        for output in export.transaction.outputs {
            st.add_output(output)?;
        }
        if export.transaction.finalized {
            st.finalize()?;
        }

        Ok(st)
    }

    /// get the identifier associated to the given `StagingTransaction`
    pub fn id(&self) -> &StagingId {
        &self.id
    }

    /// get a reference to the operations. It is not posible to have
    /// a mutable reference as we need to update other components
    /// at the same time (like the `StagingTransaction`'s file).
    pub fn operations(&self) -> &[Operation] {
        &self.operations
    }

    /// get the transaction
    pub fn transaction(&self) -> &Transaction {
        &self.transaction
    }

    /// tell of the transaction is finalized and needs to be signed now
    pub fn is_finalized(&self) -> bool {
        self.transaction.is_finalized()
    }

    /// retrieve a `StagingTransaction` from the given staging id. It will
    /// try to lock the staging file, to parse it and apply every operations
    /// one by one
    ///
    /// # Error
    ///
    /// 1. the staging file is already locked (opened for read/write) by another
    ///    thread/process (or the same process);
    /// 2. the data is unsupported or corrupted;
    ///
    pub fn read_from_file(
        root_dir: PathBuf,
        id: StagingId,
    ) -> Result<Self, StagingTransactionParseError> {
        let path = config::transaction_file(root_dir, id);
        let lock = Lock::lock(path)?;
        let mut reader = append::Reader::open(lock)?;

        // check the staging transaction magic
        let magic_got = reader.next()?;
        match magic_got {
            None => return Err(StagingTransactionParseError::NoMagic),
            Some(magic_got) => {
                if magic_got != MAGIC_TRANSACTION_V1 {
                    return Err(StagingTransactionParseError::InvalidMagic(magic_got));
                }
            }
        }
        let protocol_magic = reader.next()?;
        let protocol_magic = match protocol_magic {
            None => return Err(StagingTransactionParseError::MissingProtocolMagic),
            Some(protocol_magic) => {
                ProtocolMagic::from(serialize::utils::read_u32(&mut protocol_magic.as_slice())?)
            }
        };

        let mut operations = Vec::new();
        let mut transaction = Transaction::new();

        while let Some(operation) = reader.next()? {
            let operation = Operation::deserialize(&operation)?;
            operations.push(operation.clone());
            transaction.update_with(operation)?;
        }

        let w = append::Writer::open(reader.close())?;

        Ok(StagingTransaction {
            id: id,
            protocol_magic: protocol_magic,
            operations: operations,
            transaction: transaction,
            writer: w,
        })
    }

    /// update the `StagingTransaction` with the given operation
    ///
    /// This function updates (in the order):
    ///
    /// 1. the staging file;
    /// 2. the transaction;
    /// 3. the in-memory list of operations
    ///
    fn append(&mut self, transaction_op: Operation) -> Result<(), StagingUpdateError> {
        self.transaction.update_with(transaction_op.clone())?;
        self.writer.append_bytes(&transaction_op.serialize())?;
        self.operations.push(transaction_op);
        Ok(())
    }

    pub fn finalize(&mut self) -> Result<(), StagingUpdateError> {
        self.append(Operation::Finalize)
    }

    pub fn add_signature(&mut self, signature: TxInWitness) -> Result<(), StagingUpdateError> {
        self.append(Operation::Signature(signature))
    }

    /// add the given input to the transaction
    ///
    /// # panic
    ///
    /// This function will panic if there is an attempt to double spend
    /// funds.
    ///
    pub fn add_input(&mut self, input: Input) -> Result<(), StagingUpdateError> {
        self.append(Operation::AddInput(input))
    }

    pub fn add_change(&mut self, change: Change) -> Result<(), StagingUpdateError> {
        self.append(Operation::AddChange(change))
    }

    pub fn add_output(&mut self, output: Output) -> Result<(), StagingUpdateError> {
        // we don't need to check anything here, we don't mind
        // reusing twice the same address/output
        self.append(Operation::AddOutput(output))
    }

    /// remove the input associated to the given `TxIn`
    ///
    /// # panic
    ///
    /// This function will panic if the TxIn does not match any inputs
    ///
    pub fn remove_input(&mut self, txin: TxoPointer) -> Result<(), StagingUpdateError> {
        self.append(Operation::RemoveInput(txin))
    }

    /// remove the input associated to the given `TxIn`
    ///
    /// # panic
    ///
    /// This function will panic if the TxIn does not match any inputs
    ///
    pub fn remove_change(&mut self, address: ExtendedAddr) -> Result<(), StagingUpdateError> {
        self.append(Operation::RemoveChange(address))
    }

    /// remove the output at the given index
    ///
    /// # panic
    ///
    /// This function will panic if the index is out of bound
    /// (i.e. if there is no output at the given index).
    ///
    pub fn remove_output(&mut self, index: u32) -> Result<(), StagingUpdateError> {
        self.append(Operation::RemoveOutput(index))
    }

    /// remove every output associated to the given address
    pub fn remove_outputs_for(&mut self, address: &ExtendedAddr) -> Result<(), StagingUpdateError> {
        let mut index = 0;

        while index != self.transaction.outputs().len() {
            assert!(
                index < u32::max_value() as usize,
                "There is clearly too many outputs in this staging transaction"
            );
            if &self.transaction.outputs()[index].address == address {
                self.remove_output(index as u32)?;
            } else {
                index += 1;
            }
        }

        Ok(())
    }
}

#[derive(Debug)]
pub enum StagingUpdateError {
    AppendFile(append::Error),
    TransactionIsInvalidState(transaction::Error),
}
impl From<append::Error> for StagingUpdateError {
    fn from(e: append::Error) -> Self {
        StagingUpdateError::AppendFile(e)
    }
}
impl From<transaction::Error> for StagingUpdateError {
    fn from(e: transaction::Error) -> Self {
        StagingUpdateError::TransactionIsInvalidState(e)
    }
}
impl fmt::Display for StagingUpdateError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            StagingUpdateError::AppendFile(_) => {
                write!(f, "Staging transaction corrupted or unsupported format")
            }
            StagingUpdateError::TransactionIsInvalidState(_) => {
                write!(f, "Invalid operation on transaction")
            }
        }
    }
}
impl error::Error for StagingUpdateError {
    fn cause(&self) -> Option<&error::Error> {
        match self {
            StagingUpdateError::AppendFile(ref err) => Some(err),
            StagingUpdateError::TransactionIsInvalidState(ref err) => Some(err),
        }
    }
}

#[derive(Debug)]
pub enum StagingTransactionParseError {
    /// low level append file error
    ///
    /// It could mean there is already a lock on the `StagingTransaction`'s file
    AppendFile(append::Error),

    /// the `StagingTransaction`'s file has no magic, this is certainly an error,
    /// a corrupted of the file or an unsupported staging transaction file.
    NoMagic,

    /// error happens when we are missing a protocol magic from the staging file
    MissingProtocolMagic,

    /// Expected a magic transaction identifier, but received the following bytes
    /// instead
    InvalidMagic(Vec<u8>),

    /// error while parsing an operation
    Operation(ParsingOperationError),

    TransactionIsInvalidState(transaction::Error),
}
impl From<ParsingOperationError> for StagingTransactionParseError {
    fn from(e: ParsingOperationError) -> Self {
        StagingTransactionParseError::Operation(e)
    }
}
impl From<::std::io::Error> for StagingTransactionParseError {
    fn from(e: ::std::io::Error) -> Self {
        StagingTransactionParseError::AppendFile(append::Error::IoError(e))
    }
}
impl From<lock::Error> for StagingTransactionParseError {
    fn from(e: lock::Error) -> Self {
        StagingTransactionParseError::AppendFile(append::Error::LockError(e))
    }
}
impl From<append::Error> for StagingTransactionParseError {
    fn from(e: append::Error) -> Self {
        StagingTransactionParseError::AppendFile(e)
    }
}
impl From<transaction::Error> for StagingTransactionParseError {
    fn from(e: transaction::Error) -> StagingTransactionParseError {
        StagingTransactionParseError::TransactionIsInvalidState(e)
    }
}

impl fmt::Display for StagingTransactionParseError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            StagingTransactionParseError::AppendFile(_) => {
                write!(f, "Staging transaction corrupted or unsupported format")
            }
            StagingTransactionParseError::NoMagic => {
                write!(f, "Staging file is missing its MagicNumber")
            }
            StagingTransactionParseError::MissingProtocolMagic => {
                write!(f, "Staging file is missing the protocol magic")
            }
            StagingTransactionParseError::InvalidMagic(_) => write!(f, "Invalid magic"),
            StagingTransactionParseError::Operation(_) => write!(
                f,
                "Error in the sequence of operation in the staging transation file"
            ),
            StagingTransactionParseError::TransactionIsInvalidState(_) => {
                write!(f, "The staging transaction is in an invalid state")
            }
        }
    }
}
impl error::Error for StagingTransactionParseError {
    fn cause(&self) -> Option<&error::Error> {
        match self {
            StagingTransactionParseError::AppendFile(ref err) => Some(err),
            StagingTransactionParseError::NoMagic => None,
            StagingTransactionParseError::MissingProtocolMagic => None,
            StagingTransactionParseError::InvalidMagic(_) => None,
            StagingTransactionParseError::Operation(ref err) => Some(err),
            StagingTransactionParseError::TransactionIsInvalidState(ref err) => Some(err),
        }
    }
}

/// staging transaction export
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Export {
    staging_id: StagingId,
    magic: String,
    protocol_magic: ProtocolMagic,
    transaction: Transaction,
}
impl From<StagingTransaction> for Export {
    fn from(st: StagingTransaction) -> Self {
        Export {
            staging_id: st.id,
            protocol_magic: st.protocol_magic,
            magic: hex::encode(MAGIC_TRANSACTION_V1),
            transaction: st.transaction,
        }
    }
}
impl<'a> From<&'a StagingTransaction> for Export {
    fn from(st: &'a StagingTransaction) -> Self {
        Export {
            staging_id: st.id,
            protocol_magic: st.protocol_magic,
            magic: hex::encode(MAGIC_TRANSACTION_V1),
            transaction: st.transaction.clone(),
        }
    }
}
