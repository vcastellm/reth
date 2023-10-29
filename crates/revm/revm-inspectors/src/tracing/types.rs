//! Types for representing call trace items.

use crate::tracing::{config::TraceStyle, utils::convert_memory};
use alloy_sol_types::decode_revert_reason;
use reth_primitives::{Address, Bytes, B256, U256, U64};
use reth_rpc_types::trace::{
    geth::{CallFrame, CallLogFrame, GethDefaultTracingOptions, StructLog},
    parity::{
        Action, ActionType, CallAction, CallOutput, CallType, CreateAction, CreateOutput,
        SelfdestructAction, TraceOutput, TransactionTrace,
    },
};
use revm::interpreter::{
    opcode, CallContext, CallScheme, CreateScheme, InstructionResult, OpCode, Stack,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};

/// A unified representation of a call
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
#[allow(missing_docs)]
pub enum CallKind {
    #[default]
    Call,
    StaticCall,
    CallCode,
    DelegateCall,
    Create,
    Create2,
}

impl CallKind {
    /// Returns true if the call is a create
    #[inline]
    pub fn is_any_create(&self) -> bool {
        matches!(self, CallKind::Create | CallKind::Create2)
    }

    /// Returns true if the call is a delegate of some sorts
    #[inline]
    pub fn is_delegate(&self) -> bool {
        matches!(self, CallKind::DelegateCall | CallKind::CallCode)
    }

    /// Returns true if the call is [CallKind::StaticCall].
    #[inline]
    pub fn is_static_call(&self) -> bool {
        matches!(self, CallKind::StaticCall)
    }
}

impl std::fmt::Display for CallKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallKind::Call => {
                write!(f, "CALL")
            }
            CallKind::StaticCall => {
                write!(f, "STATICCALL")
            }
            CallKind::CallCode => {
                write!(f, "CALLCODE")
            }
            CallKind::DelegateCall => {
                write!(f, "DELEGATECALL")
            }
            CallKind::Create => {
                write!(f, "CREATE")
            }
            CallKind::Create2 => {
                write!(f, "CREATE2")
            }
        }
    }
}

impl From<CallScheme> for CallKind {
    fn from(scheme: CallScheme) -> Self {
        match scheme {
            CallScheme::Call => CallKind::Call,
            CallScheme::StaticCall => CallKind::StaticCall,
            CallScheme::CallCode => CallKind::CallCode,
            CallScheme::DelegateCall => CallKind::DelegateCall,
        }
    }
}

impl From<CreateScheme> for CallKind {
    fn from(create: CreateScheme) -> Self {
        match create {
            CreateScheme::Create => CallKind::Create,
            CreateScheme::Create2 { .. } => CallKind::Create2,
        }
    }
}

impl From<CallKind> for ActionType {
    fn from(kind: CallKind) -> Self {
        match kind {
            CallKind::Call | CallKind::StaticCall | CallKind::DelegateCall | CallKind::CallCode => {
                ActionType::Call
            }
            CallKind::Create => ActionType::Create,
            CallKind::Create2 => ActionType::Create,
        }
    }
}

impl From<CallKind> for CallType {
    fn from(ty: CallKind) -> Self {
        match ty {
            CallKind::Call => CallType::Call,
            CallKind::StaticCall => CallType::StaticCall,
            CallKind::CallCode => CallType::CallCode,
            CallKind::DelegateCall => CallType::DelegateCall,
            CallKind::Create => CallType::None,
            CallKind::Create2 => CallType::None,
        }
    }
}

/// A trace of a call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CallTrace {
    /// The depth of the call
    pub(crate) depth: usize,
    /// Whether the call was successful
    pub(crate) success: bool,
    /// caller of this call
    pub(crate) caller: Address,
    /// The destination address of the call or the address from the created contract.
    ///
    /// In other words, this is the callee if the [CallKind::Call] or the address of the created
    /// contract if [CallKind::Create].
    pub(crate) address: Address,
    /// Whether this is a call to a precompile
    ///
    /// Note: This is an Option because not all tracers make use of this
    pub(crate) maybe_precompile: Option<bool>,
    /// Holds the target for the selfdestruct refund target if `status` is
    /// [InstructionResult::SelfDestruct]
    pub(crate) selfdestruct_refund_target: Option<Address>,
    /// The kind of call this is
    pub(crate) kind: CallKind,
    /// The value transferred in the call
    pub(crate) value: U256,
    /// The calldata for the call, or the init code for contract creations
    pub(crate) data: Bytes,
    /// The return data of the call if this was not a contract creation, otherwise it is the
    /// runtime bytecode of the created contract
    pub(crate) output: Bytes,
    /// The gas cost of the call
    pub(crate) gas_used: u64,
    /// The gas limit of the call
    pub(crate) gas_limit: u64,
    /// The status of the trace's call
    pub(crate) status: InstructionResult,
    /// call context of the runtime
    pub(crate) call_context: Option<CallContext>,
    /// Opcode-level execution steps
    pub(crate) steps: Vec<CallTraceStep>,
}

impl CallTrace {
    // Returns true if the status code is an error or revert, See [InstructionResult::Revert]
    pub(crate) fn is_error(&self) -> bool {
        self.status as u8 >= InstructionResult::Revert as u8
    }

    // Returns true if the status code is a revert
    pub(crate) fn is_revert(&self) -> bool {
        self.status == InstructionResult::Revert
    }

    /// Returns the error message if it is an erroneous result.
    pub(crate) fn as_error(&self, kind: TraceStyle) -> Option<String> {
        // See also <https://github.com/ethereum/go-ethereum/blob/34d507215951fb3f4a5983b65e127577989a6db8/eth/tracers/native/call_flat.go#L39-L55>
        self.is_error().then(|| match self.status {
            InstructionResult::Revert => {
                if kind.is_parity() { "Reverted" } else { "execution reverted" }.to_string()
            }
            InstructionResult::OutOfGas | InstructionResult::MemoryOOG => {
                if kind.is_parity() { "Out of gas" } else { "out of gas" }.to_string()
            }
            InstructionResult::OpcodeNotFound => {
                if kind.is_parity() { "Bad instruction" } else { "invalid opcode" }.to_string()
            }
            InstructionResult::StackOverflow => "Out of stack".to_string(),
            InstructionResult::InvalidJump => {
                if kind.is_parity() { "Bad jump destination" } else { "invalid jump destination" }
                    .to_string()
            }
            InstructionResult::PrecompileError => {
                if kind.is_parity() { "Built-in failed" } else { "precompiled failed" }.to_string()
            }
            status => format!("{:?}", status),
        })
    }
}

impl Default for CallTrace {
    fn default() -> Self {
        Self {
            depth: Default::default(),
            success: Default::default(),
            caller: Default::default(),
            address: Default::default(),
            selfdestruct_refund_target: None,
            kind: Default::default(),
            value: Default::default(),
            data: Default::default(),
            maybe_precompile: None,
            output: Default::default(),
            gas_used: Default::default(),
            gas_limit: Default::default(),
            status: InstructionResult::Continue,
            call_context: Default::default(),
            steps: Default::default(),
        }
    }
}

/// A node in the arena
#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub(crate) struct CallTraceNode {
    /// Parent node index in the arena
    pub(crate) parent: Option<usize>,
    /// Children node indexes in the arena
    pub(crate) children: Vec<usize>,
    /// This node's index in the arena
    pub(crate) idx: usize,
    /// The call trace
    pub(crate) trace: CallTrace,
    /// Logs
    pub(crate) logs: Vec<RawLog>,
    /// Ordering of child calls and logs
    pub(crate) ordering: Vec<LogCallOrder>,
}

impl CallTraceNode {
    /// Returns the call context's execution address
    ///
    /// See `Inspector::call` impl of [TracingInspector](crate::tracing::TracingInspector)
    pub(crate) fn execution_address(&self) -> Address {
        if self.trace.kind.is_delegate() {
            self.trace.caller
        } else {
            self.trace.address
        }
    }

    /// Returns all storage slots touched by this trace and the value this storage.
    ///
    /// A touched slot is either a slot that was written to or read from.
    ///
    /// If the slot is accessed more than once, the result only includes the first time it was
    /// accessed, in other words in only returns the original value of the slot.
    pub(crate) fn touched_slots(&self) -> BTreeMap<U256, U256> {
        let mut touched_slots = BTreeMap::new();
        for change in self.trace.steps.iter().filter_map(|s| s.storage_change.as_ref()) {
            match touched_slots.entry(change.key) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(change.value);
                }
                std::collections::btree_map::Entry::Occupied(_) => {
                    // already touched
                }
            }
        }

        touched_slots
    }

    /// Pushes all steps onto the stack in reverse order
    /// so that the first step is on top of the stack
    pub(crate) fn push_steps_on_stack<'a>(
        &'a self,
        stack: &mut VecDeque<CallTraceStepStackItem<'a>>,
    ) {
        stack.extend(self.call_step_stack().into_iter().rev());
    }

    /// Returns a list of all steps in this trace in the order they were executed
    ///
    /// If the step is a call, the id of the child trace is set.
    pub(crate) fn call_step_stack(&self) -> Vec<CallTraceStepStackItem<'_>> {
        let mut stack = Vec::with_capacity(self.trace.steps.len());
        let mut child_id = 0;
        for step in self.trace.steps.iter() {
            let mut item = CallTraceStepStackItem { trace_node: self, step, call_child_id: None };

            // If the opcode is a call, put the child trace on the stack
            if step.is_calllike_op() {
                // The opcode of this step is a call but it's possible that this step resulted
                // in a revert or out of gas error in which case there's no actual child call executed and recorded: <https://github.com/paradigmxyz/reth/issues/3915>
                if let Some(call_id) = self.children.get(child_id).copied() {
                    item.call_child_id = Some(call_id);
                    child_id += 1;
                }
            }
            stack.push(item);
        }
        stack
    }

    /// Returns true if this is a call to a precompile
    #[inline]
    pub(crate) fn is_precompile(&self) -> bool {
        self.trace.maybe_precompile.unwrap_or(false)
    }

    /// Returns the kind of call the trace belongs to
    pub(crate) fn kind(&self) -> CallKind {
        self.trace.kind
    }

    /// Returns the status of the call
    pub(crate) fn status(&self) -> InstructionResult {
        self.trace.status
    }

    /// Returns true if the call was a selfdestruct
    #[inline]
    pub(crate) fn is_selfdestruct(&self) -> bool {
        self.status() == InstructionResult::SelfDestruct
    }

    /// Converts this node into a parity `TransactionTrace`
    pub(crate) fn parity_transaction_trace(&self, trace_address: Vec<usize>) -> TransactionTrace {
        let action = self.parity_action();
        let result = if self.trace.is_error() && !self.trace.is_revert() {
            // if the trace is a selfdestruct or an error that is not a revert, the result is None
            None
        } else {
            Some(self.parity_trace_output())
        };
        let error = self.trace.as_error(TraceStyle::Parity);
        TransactionTrace { action, error, result, trace_address, subtraces: self.children.len() }
    }

    /// Returns the `Output` for a parity trace
    pub(crate) fn parity_trace_output(&self) -> TraceOutput {
        match self.kind() {
            CallKind::Call | CallKind::StaticCall | CallKind::CallCode | CallKind::DelegateCall => {
                TraceOutput::Call(CallOutput {
                    gas_used: U64::from(self.trace.gas_used),
                    output: self.trace.output.clone(),
                })
            }
            CallKind::Create | CallKind::Create2 => TraceOutput::Create(CreateOutput {
                gas_used: U64::from(self.trace.gas_used),
                code: self.trace.output.clone(),
                address: self.trace.address,
            }),
        }
    }

    /// If the trace is a selfdestruct, returns the `Action` for a parity trace.
    pub(crate) fn parity_selfdestruct_action(&self) -> Option<Action> {
        if self.is_selfdestruct() {
            Some(Action::Selfdestruct(SelfdestructAction {
                address: self.trace.address,
                refund_address: self.trace.selfdestruct_refund_target.unwrap_or_default(),
                balance: self.trace.value,
            }))
        } else {
            None
        }
    }

    /// If the trace is a selfdestruct, returns the `CallFrame` for a geth call trace
    pub(crate) fn geth_selfdestruct_call_trace(&self) -> Option<CallFrame> {
        if self.is_selfdestruct() {
            Some(CallFrame {
                typ: "SELFDESTRUCT".to_string(),
                from: self.trace.caller,
                to: self.trace.selfdestruct_refund_target,
                value: Some(self.trace.value),
                ..Default::default()
            })
        } else {
            None
        }
    }

    /// If the trace is a selfdestruct, returns the `TransactionTrace` for a parity trace.
    pub(crate) fn parity_selfdestruct_trace(
        &self,
        trace_address: Vec<usize>,
    ) -> Option<TransactionTrace> {
        let trace = self.parity_selfdestruct_action()?;
        Some(TransactionTrace {
            action: trace,
            error: None,
            result: None,
            trace_address,
            subtraces: 0,
        })
    }

    /// Returns the `Action` for a parity trace.
    ///
    /// Caution: This does not include the selfdestruct action, if the trace is a selfdestruct,
    /// since those are handled in addition to the call action.
    pub(crate) fn parity_action(&self) -> Action {
        match self.kind() {
            CallKind::Call | CallKind::StaticCall | CallKind::CallCode | CallKind::DelegateCall => {
                Action::Call(CallAction {
                    from: self.trace.caller,
                    to: self.trace.address,
                    value: self.trace.value,
                    gas: U64::from(self.trace.gas_limit),
                    input: self.trace.data.clone(),
                    call_type: self.kind().into(),
                })
            }
            CallKind::Create | CallKind::Create2 => Action::Create(CreateAction {
                from: self.trace.caller,
                value: self.trace.value,
                gas: U64::from(self.trace.gas_limit),
                init: self.trace.data.clone(),
            }),
        }
    }

    /// Converts this call trace into an _empty_ geth [CallFrame]
    ///
    /// Caution: this does not include any of the child calls
    pub(crate) fn geth_empty_call_frame(&self, include_logs: bool) -> CallFrame {
        let mut call_frame = CallFrame {
            typ: self.trace.kind.to_string(),
            from: self.trace.caller,
            to: Some(self.trace.address),
            value: Some(self.trace.value),
            gas: U256::from(self.trace.gas_limit),
            gas_used: U256::from(self.trace.gas_used),
            input: self.trace.data.clone(),
            output: (!self.trace.output.is_empty()).then(|| self.trace.output.clone()),
            error: None,
            revert_reason: None,
            calls: Default::default(),
            logs: Default::default(),
        };

        if self.trace.kind.is_static_call() {
            // STATICCALL frames don't have a value
            call_frame.value = None;
        }

        // we need to populate error and revert reason
        if !self.trace.success {
            call_frame.revert_reason = decode_revert_reason(self.trace.output.as_ref());
            // Note: the call tracer mimics parity's trace transaction and geth maps errors to parity style error messages, <https://github.com/ethereum/go-ethereum/blob/34d507215951fb3f4a5983b65e127577989a6db8/eth/tracers/native/call_flat.go#L39-L55>
            call_frame.error = self.trace.as_error(TraceStyle::Parity);
        }

        if include_logs && !self.logs.is_empty() {
            call_frame.logs = self
                .logs
                .iter()
                .map(|log| CallLogFrame {
                    address: Some(self.execution_address()),
                    topics: Some(log.topics.clone()),
                    data: Some(log.data.clone()),
                })
                .collect();
        }

        call_frame
    }
}

pub(crate) struct CallTraceStepStackItem<'a> {
    /// The trace node that contains this step
    pub(crate) trace_node: &'a CallTraceNode,
    /// The step that this stack item represents
    pub(crate) step: &'a CallTraceStep,
    /// The index of the child call in the CallArena if this step's opcode is a call
    pub(crate) call_child_id: Option<usize>,
}

/// Ordering enum for calls and logs
///
/// i.e. if Call 0 occurs before Log 0, it will be pushed into the `CallTraceNode`'s ordering before
/// the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LogCallOrder {
    Log(usize),
    Call(usize),
}

/// Ethereum log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawLog {
    /// Indexed event params are represented as log topics.
    pub(crate) topics: Vec<B256>,
    /// Others are just plain data.
    pub(crate) data: Bytes,
}

/// Represents a tracked call step during execution
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CallTraceStep {
    // Fields filled in `step`
    /// Call depth
    pub(crate) depth: u64,
    /// Program counter before step execution
    pub(crate) pc: usize,
    /// Opcode to be executed
    pub(crate) op: OpCode,
    /// Current contract address
    pub(crate) contract: Address,
    /// Stack before step execution
    pub(crate) stack: Stack,
    /// The new stack items placed by this step if any
    pub(crate) push_stack: Option<Vec<U256>>,
    /// All allocated memory in a step
    ///
    /// This will be empty if memory capture is disabled
    pub(crate) memory: RecordedMemory,
    /// Size of memory at the beginning of the step
    pub(crate) memory_size: usize,
    /// Remaining gas before step execution
    pub(crate) gas_remaining: u64,
    /// Gas refund counter before step execution
    pub(crate) gas_refund_counter: u64,
    // Fields filled in `step_end`
    /// Gas cost of step execution
    pub(crate) gas_cost: u64,
    /// Change of the contract state after step execution (effect of the SLOAD/SSTORE instructions)
    pub(crate) storage_change: Option<StorageChange>,
    /// Final status of the step
    ///
    /// This is set after the step was executed.
    pub(crate) status: InstructionResult,
}

// === impl CallTraceStep ===

impl CallTraceStep {
    /// Converts this step into a geth [StructLog]
    ///
    /// This sets memory and stack capture based on the `opts` parameter.
    pub(crate) fn convert_to_geth_struct_log(&self, opts: &GethDefaultTracingOptions) -> StructLog {
        let mut log = StructLog {
            depth: self.depth,
            error: self.as_error(),
            gas: self.gas_remaining,
            gas_cost: self.gas_cost,
            op: self.op.to_string(),
            pc: self.pc as u64,
            refund_counter: (self.gas_refund_counter > 0).then_some(self.gas_refund_counter),
            // Filled, if not disabled manually
            stack: None,
            // Filled in `CallTraceArena::geth_trace` as a result of compounding all slot changes
            return_data: None,
            // Filled via trace object
            storage: None,
            // Only enabled if `opts.enable_memory` is true
            memory: None,
            // This is None in the rpc response
            memory_size: None,
        };

        if opts.is_stack_enabled() {
            log.stack = Some(self.stack.data().clone());
        }

        if opts.is_memory_enabled() {
            log.memory = Some(self.memory.memory_chunks());
        }

        log
    }

    /// Returns true if the step is a STOP opcode
    #[inline]
    pub(crate) fn is_stop(&self) -> bool {
        matches!(self.op.get(), opcode::STOP)
    }

    /// Returns true if the step is a call operation, any of
    /// CALL, CALLCODE, DELEGATECALL, STATICCALL, CREATE, CREATE2
    #[inline]
    pub(crate) fn is_calllike_op(&self) -> bool {
        matches!(
            self.op.get(),
            opcode::CALL |
                opcode::DELEGATECALL |
                opcode::STATICCALL |
                opcode::CREATE |
                opcode::CALLCODE |
                opcode::CREATE2
        )
    }

    // Returns true if the status code is an error or revert, See [InstructionResult::Revert]
    pub(crate) fn is_error(&self) -> bool {
        self.status as u8 >= InstructionResult::Revert as u8
    }

    /// Returns the error message if it is an erroneous result.
    pub(crate) fn as_error(&self) -> Option<String> {
        self.is_error().then(|| format!("{:?}", self.status))
    }
}

/// Represents the source of a storage change - e.g., whether it came
/// from an SSTORE or SLOAD instruction.
#[allow(clippy::upper_case_acronyms)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StorageChangeReason {
    SLOAD,
    SSTORE,
}

/// Represents a storage change during execution
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StorageChange {
    pub(crate) key: U256,
    pub(crate) value: U256,
    pub(crate) had_value: Option<U256>,
    pub(crate) reason: StorageChangeReason,
}

/// Represents the memory captured during execution
///
/// This is a wrapper around the [SharedMemory](revm::interpreter::SharedMemory) context memory.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct RecordedMemory(pub(crate) Vec<u8>);

impl RecordedMemory {
    pub(crate) fn new(mem: Vec<u8>) -> Self {
        Self(mem)
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub(crate) fn resize(&mut self, size: usize) {
        self.0.resize(size, 0);
    }

    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Converts the memory into 32byte hex chunks
    pub(crate) fn memory_chunks(&self) -> Vec<String> {
        convert_memory(self.as_bytes())
    }
}
