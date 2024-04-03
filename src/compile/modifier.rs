//! Compiler code for modifiers

use std::slice;

use crate::format::format_words;

use super::*;

impl Compiler {
    #[allow(clippy::collapsible_match)]
    pub(super) fn modified(&mut self, mut modified: Modified, call: bool) -> UiuaResult {
        let mut op_count = modified.code_operands().count();

        // De-sugar function pack
        if op_count == 1 {
            let operand = modified.code_operands().next().unwrap().clone();
            if let Sp {
                value: Word::Switch(sw @ Switch { angled: false, .. }),
                span,
            } = operand
            {
                match &modified.modifier.value {
                    Modifier::Primitive(Primitive::Dip) => {
                        let mut branches = sw.branches.into_iter().rev();
                        let mut new = Modified {
                            modifier: modified.modifier.clone(),
                            operands: vec![branches.next().unwrap().map(Word::Func)],
                        };
                        for branch in branches {
                            let mut lines = branch.value.lines;
                            (lines.last_mut().unwrap())
                                .push(span.clone().sp(Word::Modified(Box::new(new))));
                            new = Modified {
                                modifier: modified.modifier.clone(),
                                operands: vec![branch.span.clone().sp(Word::Func(Func {
                                    id: FunctionId::Anonymous(branch.span.clone()),
                                    signature: None,
                                    lines,
                                    closed: true,
                                }))],
                            };
                        }
                        return self.modified(new, call);
                    }
                    Modifier::Primitive(
                        Primitive::Fork | Primitive::Bracket | Primitive::Try | Primitive::Fill,
                    ) => {
                        let mut branches = sw.branches.into_iter().rev();
                        let mut new = Modified {
                            modifier: modified.modifier.clone(),
                            operands: {
                                let mut ops: Vec<_> = branches
                                    .by_ref()
                                    .take(2)
                                    .map(|w| w.map(Word::Func))
                                    .collect();
                                ops.reverse();
                                ops
                            },
                        };
                        for branch in branches {
                            new = Modified {
                                modifier: modified.modifier.clone(),
                                operands: vec![
                                    branch.map(Word::Func),
                                    span.clone().sp(Word::Modified(Box::new(new))),
                                ],
                            };
                        }
                        return self.modified(new, call);
                    }
                    Modifier::Primitive(Primitive::Cascade) => {
                        let mut branches = sw.branches.into_iter().rev();
                        let mut new = Modified {
                            modifier: modified.modifier.clone(),
                            operands: {
                                let mut ops: Vec<_> = branches
                                    .by_ref()
                                    .take(2)
                                    .map(|w| w.map(Word::Func))
                                    .collect();
                                ops.reverse();
                                ops
                            },
                        };
                        for branch in branches {
                            new = Modified {
                                modifier: modified.modifier.clone(),
                                operands: vec![
                                    branch.map(Word::Func),
                                    span.clone().sp(Word::Modified(Box::new(new))),
                                ],
                            };
                        }
                        return self.modified(new, call);
                    }
                    modifier if modifier.args() >= 2 => {
                        if sw.branches.len() != modifier.args() {
                            return Err(self.fatal_error(
                                modified.modifier.span.clone().merge(span),
                                format!(
                                    "{} requires {} function arguments, but the \
                                    function pack has {} functions",
                                    modifier,
                                    modifier.args(),
                                    sw.branches.len()
                                ),
                            ));
                        }
                        let new = Modified {
                            modifier: modified.modifier.clone(),
                            operands: sw.branches.into_iter().map(|w| w.map(Word::Func)).collect(),
                        };
                        return self.modified(new, call);
                    }
                    modifier => 'blk: {
                        if let Modifier::Ref(name) = modifier {
                            if let Ok((_, local)) = self.ref_local(name) {
                                if self.array_macros.contains_key(&local.index) {
                                    break 'blk;
                                }
                            }
                        }
                        return Err(self.fatal_error(
                            modified.modifier.span.clone().merge(span),
                            format!(
                                "{modifier} cannot use a function pack. If you meant to \
                                use a switch function, add a layer of parentheses."
                            ),
                        ));
                    }
                }
            }
        }

        if op_count < modified.modifier.value.args() {
            let missing = modified.modifier.value.args() - op_count;
            let span = modified.modifier.span.clone();
            for _ in 0..missing {
                modified.operands.push(span.clone().sp(Word::Func(Func {
                    id: FunctionId::Anonymous(span.clone()),
                    signature: None,
                    lines: Vec::new(),
                    closed: false,
                })));
            }
            op_count = modified.operands.len();
        }
        if op_count == modified.modifier.value.args() {
            // Inlining
            if self.inline_modifier(&modified, call)? {
                return Ok(());
            }
        } else {
            // Validate operand count
            return Err(self.fatal_error(
                modified.modifier.span.clone(),
                format!(
                    "{} requires {} function argument{}, but {} {} provided",
                    modified.modifier.value,
                    modified.modifier.value.args(),
                    if modified.modifier.value.args() == 1 {
                        ""
                    } else {
                        "s"
                    },
                    op_count,
                    if op_count == 1 { "was" } else { "were" }
                ),
            ));
        }

        // Handle macros
        let prim = match modified.modifier.value {
            Modifier::Primitive(prim) => prim,
            Modifier::Ref(r) => {
                let (path_locals, local) = self.ref_local(&r)?;
                self.validate_local(&r.name.value, local, &r.name.span);
                self.code_meta
                    .global_references
                    .insert(r.name.clone(), local.index);
                for (local, comp) in path_locals.into_iter().zip(&r.path) {
                    (self.code_meta.global_references).insert(comp.module.clone(), local.index);
                }
                // Handle recursion depth
                self.macro_depth += 1;
                if self.macro_depth > 20 {
                    return Err(
                        self.fatal_error(modified.modifier.span.clone(), "Macro recurs too deep")
                    );
                }
                if let Some(mut mac) = self.stack_macros.get(&local.index).cloned() {
                    // Stack macros
                    // Expand
                    self.expand_macro(
                        r.name.value.clone(),
                        &mut mac.words,
                        modified.operands,
                        modified.modifier.span.clone(),
                    )?;
                    // Compile
                    let instrs =
                        self.temp_scope(mac.names, |comp| comp.compile_words(mac.words, true))?;
                    // Add
                    let sig = self.sig_of(&instrs, &modified.modifier.span)?;
                    let func =
                        self.make_function(FunctionId::Named(r.name.value.clone()), sig, instrs);
                    self.push_instr(Instr::PushFunc(func));
                    if call {
                        let span = self.add_span(modified.modifier.span);
                        self.push_instr(Instr::Call(span));
                    }
                } else if let Some(mac) = self.array_macros.get(&local.index).cloned() {
                    // Array macros
                    let full_span = (modified.modifier.span.clone())
                        .merge(modified.operands.last().unwrap().span.clone());

                    // Collect operands as strings
                    let mut operands: Vec<Sp<Word>> = (modified.operands.into_iter())
                        .filter(|w| w.value.is_code())
                        .collect();
                    if operands.len() == 1 {
                        let operand = operands.remove(0);
                        operands = match operand.value {
                            Word::Switch(sw) => {
                                sw.branches.into_iter().map(|b| b.map(Word::Func)).collect()
                            }
                            word => vec![operand.span.sp(word)],
                        };
                    }
                    let op_sigs = if mac.function.signature().args == 2 {
                        let mut comp = self.clone();
                        let mut sig_data: EcoVec<u8> = EcoVec::with_capacity(operands.len() * 2);
                        for op in &operands {
                            let (_, sig) = comp.compile_operand_word(op.clone())?;
                            sig_data.extend_from_slice(&[sig.args as u8, sig.outputs as u8]);
                        }
                        Some(Array::<u8>::new([operands.len(), 2], sig_data))
                    } else {
                        None
                    };
                    let formatted: Array<Boxed> = operands
                        .iter()
                        .map(|w| {
                            let mut formatted = format_word(w, &self.asm.inputs);
                            if let Word::Func(_) = &w.value {
                                if formatted.starts_with('(') && formatted.ends_with(')') {
                                    formatted = formatted[1..formatted.len() - 1].to_string();
                                }
                            }
                            Boxed(formatted.trim().into())
                        })
                        .collect();

                    let mut code = String::new();
                    (|| -> UiuaResult {
                        self.prepare_env()?;
                        let env = &mut self.macro_env;
                        // Run the macro function
                        if let Some(sigs) = op_sigs {
                            env.push(sigs);
                        }
                        env.push(formatted);
                        env.call(mac.function)?;
                        let val = env.pop("macro result")?;

                        // Parse the macro output
                        if let Ok(s) = val.as_string(env, "") {
                            code = s;
                        } else {
                            for row in val.into_rows() {
                                let s = row.as_string(env, "Macro output rows must be strings")?;
                                if code.chars().last().is_some_and(|c| !c.is_whitespace()) {
                                    code.push(' ');
                                }
                                code.push_str(&s);
                            }
                        }
                        Ok(())
                    })()
                    .map_err(|e| e.trace_macro(modified.modifier.span.clone()))?;

                    // Quote
                    self.code_meta
                        .macro_expansions
                        .insert(full_span, (r.name.value.clone(), code.clone()));
                    self.temp_scope(mac.names, |comp| {
                        comp.quote(&code, &modified.modifier.span, call)
                    })?;
                } else {
                    return Err(self.fatal_error(
                        modified.modifier.span.clone(),
                        format!(
                            "Macro {} not found. This is a bug in the interpreter.",
                            r.name.value
                        ),
                    ));
                }
                self.macro_depth -= 1;

                return Ok(());
            }
        };

        // Compile operands
        let instrs = self.compile_words(modified.operands, false)?;

        if call {
            self.push_all_instrs(instrs);
            self.primitive(prim, modified.modifier.span, true)?;
        } else {
            self.new_functions.push(EcoVec::new());
            self.push_all_instrs(instrs);
            self.primitive(prim, modified.modifier.span.clone(), true)?;
            let instrs = self.new_functions.pop().unwrap();
            let sig = self.sig_of(&instrs, &modified.modifier.span)?;
            let func =
                self.make_function(FunctionId::Anonymous(modified.modifier.span), sig, instrs);
            self.push_instr(Instr::PushFunc(func));
        }
        Ok(())
    }
    pub(super) fn inline_modifier(&mut self, modified: &Modified, call: bool) -> UiuaResult<bool> {
        use Primitive::*;
        let Modifier::Primitive(prim) = modified.modifier.value else {
            return Ok(false);
        };
        macro_rules! finish {
            ($instrs:expr, $sig:expr) => {{
                if call {
                    self.push_all_instrs($instrs);
                } else {
                    let func = self.make_function(
                        FunctionId::Anonymous(modified.modifier.span.clone()),
                        $sig,
                        $instrs.to_vec(),
                    );
                    self.push_instr(Instr::PushFunc(func));
                }
            }};
        }
        match prim {
            Dip | Gap | On | By => {
                // Compile operands
                let (mut instrs, sig) = self.compile_operand_word(modified.operands[0].clone())?;
                // Dip (|1 …) . diagnostic
                if prim == Dip && sig == (1, 1) {
                    if let Some(Instr::Prim(Dup, dup_span)) =
                        self.new_functions.last().and_then(|instrs| instrs.last())
                    {
                        if let Span::Code(dup_span) = self.get_span(*dup_span) {
                            let span = modified.modifier.span.clone().merge(dup_span);
                            self.emit_diagnostic(
                                "Prefer `⟜(…)` over `⊙(…).` for clarity",
                                DiagnosticKind::Style,
                                span,
                            );
                        }
                    }
                }

                let span = self.add_span(modified.modifier.span.clone());
                let sig = match prim {
                    Dip => {
                        instrs.insert(
                            0,
                            Instr::PushTemp {
                                stack: TempStack::Inline,
                                count: 1,
                                span,
                            },
                        );
                        instrs.push(Instr::PopTemp {
                            stack: TempStack::Inline,
                            count: 1,
                            span,
                        });
                        Signature::new(sig.args + 1, sig.outputs + 1)
                    }
                    Gap => {
                        instrs.insert(0, Instr::Prim(Pop, span));
                        Signature::new(sig.args + 1, sig.outputs)
                    }
                    On => {
                        instrs.insert(
                            0,
                            if sig.args == 0 {
                                Instr::PushTemp {
                                    stack: TempStack::Inline,
                                    count: 1,
                                    span,
                                }
                            } else {
                                Instr::CopyToTemp {
                                    stack: TempStack::Inline,
                                    count: 1,
                                    span,
                                }
                            },
                        );
                        instrs.push(Instr::PopTemp {
                            stack: TempStack::Inline,
                            count: 1,
                            span,
                        });
                        Signature::new(sig.args.max(1), sig.outputs + 1)
                    }
                    By => {
                        if sig.args > 0 {
                            let mut i = 0;
                            if sig.args > 1 {
                                instrs.insert(
                                    i,
                                    Instr::PushTemp {
                                        stack: TempStack::Inline,
                                        count: sig.args - 1,
                                        span,
                                    },
                                );
                                i += 1;
                            }
                            instrs.insert(i, Instr::Prim(Dup, span));
                            i += 1;
                            if sig.args > 1 {
                                instrs.insert(
                                    i,
                                    Instr::PopTemp {
                                        stack: TempStack::Inline,
                                        count: sig.args - 1,
                                        span,
                                    },
                                );
                            }
                        }
                        Signature::new(sig.args, sig.outputs + 1)
                    }
                    _ => unreachable!(),
                };
                if call {
                    self.push_instr(Instr::PushSig(sig));
                    self.push_all_instrs(instrs);
                    self.push_instr(Instr::PopSig);
                } else {
                    let func = self.make_function(
                        FunctionId::Anonymous(modified.modifier.span.clone()),
                        sig,
                        instrs,
                    );
                    self.push_instr(Instr::PushFunc(func));
                }
            }
            Fork => {
                let mut operands = modified.code_operands().cloned();
                let first_op = operands.next().unwrap();
                // ⊃∘ diagnostic
                if let Word::Primitive(Primitive::Identity) = first_op.value {
                    self.emit_diagnostic(
                        "Prefer `⟜` over `⊃∘` for clarity",
                        DiagnosticKind::Style,
                        modified.modifier.span.clone().merge(first_op.span.clone()),
                    );
                }
                let (a_instrs, a_sig) = self.compile_operand_word(first_op)?;
                let (b_instrs, b_sig) = self.compile_operand_word(operands.next().unwrap())?;
                let span = self.add_span(modified.modifier.span.clone());
                let mut instrs = Vec::new();
                if a_sig.args > 0 {
                    instrs.push(Instr::CopyToTemp {
                        stack: TempStack::Inline,
                        count: a_sig.args,
                        span,
                    });
                }
                if a_sig.args > b_sig.args {
                    let diff = a_sig.args - b_sig.args;
                    if b_sig.args > 0 {
                        instrs.push(Instr::PushTemp {
                            stack: TempStack::Inline,
                            count: b_sig.args,
                            span,
                        });
                    }
                    for _ in 0..diff {
                        instrs.push(Instr::Prim(Pop, span));
                    }
                    if b_sig.args > 0 {
                        instrs.push(Instr::PopTemp {
                            stack: TempStack::Inline,
                            count: b_sig.args,
                            span,
                        });
                    }
                }
                instrs.extend(b_instrs);
                if a_sig.args > 0 {
                    instrs.push(Instr::PopTemp {
                        stack: TempStack::Inline,
                        count: a_sig.args,
                        span,
                    });
                }
                instrs.extend(a_instrs);
                let sig = Signature::new(a_sig.args.max(b_sig.args), a_sig.outputs + b_sig.outputs);
                if call {
                    self.push_instr(Instr::PushSig(sig));
                    self.push_all_instrs(instrs);
                    self.push_instr(Instr::PopSig);
                } else {
                    let func = self.make_function(
                        FunctionId::Anonymous(modified.modifier.span.clone()),
                        sig,
                        instrs,
                    );
                    self.push_instr(Instr::PushFunc(func));
                }
            }
            Cascade => {
                let mut operands = modified.code_operands().cloned();
                let (a_instrs, a_sig) = self.compile_operand_word(operands.next().unwrap())?;
                let (b_instrs, b_sig) = self.compile_operand_word(operands.next().unwrap())?;
                let span = self.add_span(modified.modifier.span.clone());
                let count = a_sig.args.saturating_sub(b_sig.outputs);
                if a_sig.args < b_sig.outputs {
                    self.emit_diagnostic(
                        format!(
                            "{}'s second function has more outputs \
                            than its first function has arguments, \
                            so {} is redundant here.",
                            prim.format(),
                            prim.format()
                        ),
                        DiagnosticKind::Advice,
                        modified.modifier.span.clone(),
                    );
                }
                let mut instrs = Vec::new();
                if count > 0 {
                    instrs.push(Instr::CopyToTemp {
                        stack: TempStack::Inline,
                        count,
                        span,
                    });
                }
                instrs.extend(b_instrs);
                if count > 0 {
                    instrs.push(Instr::PopTemp {
                        stack: TempStack::Inline,
                        count,
                        span,
                    });
                }
                instrs.extend(a_instrs);
                let sig = Signature::new(
                    b_sig.args.max(count),
                    a_sig.outputs.max(count.saturating_sub(b_sig.outputs)),
                );
                if call {
                    self.push_instr(Instr::PushSig(sig));
                    self.push_all_instrs(instrs);
                    self.push_instr(Instr::PopSig);
                } else {
                    let func = self.make_function(
                        FunctionId::Anonymous(modified.modifier.span.clone()),
                        sig,
                        instrs,
                    );
                    self.push_instr(Instr::PushFunc(func));
                }
            }
            Bracket => {
                let mut operands = modified.code_operands().cloned();
                let (a_instrs, a_sig) = self.compile_operand_word(operands.next().unwrap())?;
                let (b_instrs, b_sig) = self.compile_operand_word(operands.next().unwrap())?;
                let span = self.add_span(modified.modifier.span.clone());
                let mut instrs = vec![Instr::PushTemp {
                    stack: TempStack::Inline,
                    count: a_sig.args,
                    span,
                }];
                instrs.extend(b_instrs);
                instrs.push(Instr::PopTemp {
                    stack: TempStack::Inline,
                    count: a_sig.args,
                    span,
                });
                instrs.extend(a_instrs);
                let sig = Signature::new(a_sig.args + b_sig.args, a_sig.outputs + b_sig.outputs);
                if call {
                    self.push_instr(Instr::PushSig(sig));
                    self.push_all_instrs(instrs);
                    self.push_instr(Instr::PopSig);
                } else {
                    let func = self.make_function(
                        FunctionId::Anonymous(modified.modifier.span.clone()),
                        sig,
                        instrs,
                    );
                    self.push_instr(Instr::PushFunc(func));
                }
            }
            Un => {
                let mut operands = modified.code_operands().cloned();
                let f = operands.next().unwrap();
                let span = f.span.clone();
                let (instrs, _) = self.compile_operand_word(f)?;
                self.add_span(span.clone());
                if let Some(inverted) = invert_instrs(&instrs, self) {
                    let sig = self.sig_of(&inverted, &span)?;
                    if call {
                        self.push_all_instrs(inverted);
                    } else {
                        let id = FunctionId::Anonymous(modified.modifier.span.clone());
                        let func = self.make_function(id, sig, inverted);
                        self.push_instr(Instr::PushFunc(func));
                    }
                } else {
                    return Err(self.fatal_error(span, "No inverse found"));
                }
            }
            Under => {
                let mut operands = modified.code_operands().cloned();
                let f = operands.next().unwrap();
                let f_span = f.span.clone();
                let (f_instrs, _) = self.compile_operand_word(f)?;
                let (g_instrs, g_sig) = self.compile_operand_word(operands.next().unwrap())?;
                if let Some((f_before, f_after)) = under_instrs(&f_instrs, g_sig, self) {
                    let before_sig = self.sig_of(&f_before, &f_span)?;
                    let after_sig = self.sig_of(&f_after, &f_span)?;
                    let mut instrs = if call {
                        eco_vec![Instr::PushSig(before_sig)]
                    } else {
                        EcoVec::new()
                    };
                    instrs.extend(f_before);
                    if call {
                        instrs.push(Instr::PopSig);
                    }
                    instrs.extend(g_instrs);
                    if call {
                        instrs.push(Instr::PushSig(after_sig));
                    }
                    instrs.extend(f_after);
                    if call {
                        instrs.push(Instr::PopSig);
                    }
                    if call {
                        self.push_all_instrs(instrs);
                    } else {
                        let sig = self.sig_of(&instrs, &modified.modifier.span)?;
                        let func = self.make_function(
                            FunctionId::Anonymous(modified.modifier.span.clone()),
                            sig,
                            instrs,
                        );
                        self.push_instr(Instr::PushFunc(func));
                    }
                } else {
                    return Err(self.fatal_error(f_span, "No inverse found"));
                }
            }
            Both => {
                let operand = modified.code_operands().next().unwrap().clone();
                let (mut instrs, sig) = self.compile_operand_word(operand)?;
                if let [Instr::Prim(Trace, span)] = instrs.as_slice() {
                    finish!(
                        [Instr::ImplPrim(ImplPrimitive::BothTrace, *span)],
                        Signature::new(2, 2)
                    )
                } else {
                    let span = self.add_span(modified.modifier.span.clone());
                    instrs.insert(
                        0,
                        Instr::PushTemp {
                            stack: TempStack::Inline,
                            count: sig.args,
                            span,
                        },
                    );
                    instrs.push(Instr::PopTemp {
                        stack: TempStack::Inline,
                        count: sig.args,
                        span,
                    });
                    for i in 1..instrs.len() - 1 {
                        instrs.push(instrs[i].clone());
                    }
                    let sig = Signature::new(sig.args * 2, sig.outputs * 2);
                    if call {
                        self.push_instr(Instr::PushSig(sig));
                        self.push_all_instrs(instrs);
                        self.push_instr(Instr::PopSig);
                    } else {
                        let func = self.make_function(
                            FunctionId::Anonymous(modified.modifier.span.clone()),
                            sig,
                            instrs,
                        );
                        self.push_instr(Instr::PushFunc(func));
                    }
                }
            }
            Fill => {
                let mut operands = modified.code_operands().rev().cloned();
                if !call {
                    self.new_functions.push(EcoVec::new());
                }
                let mode = replace(&mut self.pre_eval_mode, PreEvalMode::Lsp);
                let res = self.word(operands.next().unwrap(), false);
                self.pre_eval_mode = mode;
                res?;
                self.word(operands.next().unwrap(), false)?;
                let span = self.add_span(modified.modifier.span.clone());
                self.push_instr(Instr::Prim(Primitive::Fill, span));
                if !call {
                    let instrs = self.new_functions.pop().unwrap();
                    let sig = self.sig_of(&instrs, &modified.modifier.span)?;
                    let func = self.make_function(
                        FunctionId::Anonymous(modified.modifier.span.clone()),
                        sig,
                        instrs,
                    );
                    self.push_instr(Instr::PushFunc(func));
                }
            }
            Try => {
                let mut operands = modified.code_operands().rev().cloned();
                if !call {
                    self.new_functions.push(EcoVec::new());
                }
                self.word(operands.next().unwrap(), false)?;
                let (tried_instrs, tried_sig) =
                    self.compile_operand_word(operands.next().unwrap())?;
                let span = self.add_span(modified.modifier.span.clone());
                let try_instr = if instrs_have_pattern_matching(&tried_instrs, &self.asm) {
                    Instr::ImplPrim(ImplPrimitive::TryPattern, span)
                } else {
                    Instr::Prim(Primitive::Try, span)
                };
                let tried_func = self.make_function(
                    FunctionId::Anonymous(modified.code_operands().next().unwrap().span.clone()),
                    tried_sig,
                    tried_instrs,
                );
                self.push_instr(Instr::PushFunc(tried_func));
                self.push_instr(try_instr);
                if !call {
                    let instrs = self.new_functions.pop().unwrap();
                    let sig = self.sig_of(&instrs, &modified.modifier.span)?;
                    let func = self.make_function(
                        FunctionId::Anonymous(modified.modifier.span.clone()),
                        sig,
                        instrs,
                    );
                    self.push_instr(Instr::PushFunc(func));
                }
            }
            Bind => {
                let operand = modified.code_operands().next().unwrap().clone();
                let operand_span = operand.span.clone();
                self.scope.bind_locals.push(HashSet::new());
                let (mut instrs, mut sig) = self.compile_operand_word(operand)?;
                let locals = self.scope.bind_locals.pop().unwrap();
                let local_count = locals.into_iter().max().map_or(0, |i| i + 1);
                let span = self.add_span(modified.modifier.span.clone());
                if local_count < 3 {
                    self.emit_diagnostic(
                        format!(
                            "{} should be reserved for functions with at least 3 arguments, \
                            but this function has {} arguments",
                            Bind.format(),
                            local_count
                        ),
                        DiagnosticKind::Advice,
                        operand_span,
                    );
                }
                instrs.insert(
                    0,
                    Instr::PushLocals {
                        count: local_count,
                        span,
                    },
                );
                instrs.push(Instr::PopLocals);
                sig.args += local_count;
                finish!(instrs, sig);
            }
            Comptime => {
                let word = modified.code_operands().next().unwrap().clone();
                self.do_comptime(prim, word, &modified.modifier.span, call)?;
            }
            Reduce => {
                // Reduce content
                let operand = modified.code_operands().next().unwrap().clone();
                let Word::Modified(m) = &operand.value else {
                    return Ok(false);
                };
                let Modifier::Primitive(Content) = &m.modifier.value else {
                    return Ok(false);
                };
                if m.code_operands().count() != 1 {
                    return Ok(false);
                }
                let operand = m.code_operands().next().unwrap().clone();
                let (content_instrs, sig) = self.compile_operand_word(operand)?;
                let content_func = self.make_function(
                    FunctionId::Anonymous(m.modifier.span.clone()),
                    sig,
                    content_instrs,
                );
                let span = self.add_span(modified.modifier.span.clone());
                let instrs = eco_vec![
                    Instr::PushFunc(content_func),
                    Instr::ImplPrim(ImplPrimitive::ReduceContent, span),
                ];
                finish!(instrs, Signature::new(1, 1));
            }
            Each => {
                // Each pervasive
                let operand = modified.code_operands().next().unwrap().clone();
                if !words_look_pervasive(slice::from_ref(&operand)) {
                    return Ok(false);
                }
                let (instrs, sig) = self.compile_operand_word(operand)?;
                let span = modified.modifier.span.clone();
                self.emit_diagnostic(
                    if let Some((prim, _)) = instrs_as_flipped_primitive(&instrs, &self.asm)
                        .filter(|(prim, _)| prim.class().is_pervasive())
                    {
                        format!(
                            "{} is pervasive, so {} is redundant here.",
                            prim.format(),
                            Each.format(),
                        )
                    } else {
                        format!(
                            "{m}'s function is pervasive, \
                        so {m} is redundant here.",
                            m = Each.format(),
                        )
                    },
                    DiagnosticKind::Advice,
                    span,
                );
                finish!(instrs, sig);
            }
            Table => {
                // Normal table compilation, but get some diagnostics
                let operand = modified.code_operands().next().unwrap().clone();
                let op_span = operand.span.clone();
                let function_id = FunctionId::Anonymous(op_span.clone());
                let (instrs, sig) = self.compile_operand_word(operand)?;
                match sig.args {
                    0 => self.emit_diagnostic(
                        format!("{} of 0 arguments is redundant", Table.format()),
                        DiagnosticKind::Advice,
                        op_span,
                    ),
                    1 => self.emit_diagnostic(
                        format!(
                            "{} with 1 argument is just {rows}. \
                            Use {rows} instead.",
                            Table.format(),
                            rows = Rows.format()
                        ),
                        DiagnosticKind::Advice,
                        op_span,
                    ),
                    _ => {}
                }
                let func = self.make_function(function_id, sig, instrs);
                let span = self.add_span(modified.modifier.span.clone());
                let instrs = [Instr::PushFunc(func), Instr::Prim(Table, span)];
                finish!(instrs, sig);
            }
            Content => {
                let operand = modified.code_operands().next().unwrap().clone();
                let (instrs, sig) = self.compile_operand_word(operand)?;
                let mut prefix = EcoVec::new();
                let span = self.add_span(modified.modifier.span.clone());
                if sig.args > 0 {
                    if sig.args > 1 {
                        prefix.push(Instr::PushTemp {
                            stack: TempStack::Inline,
                            count: sig.args - 1,
                            span,
                        });
                        for _ in 0..sig.args - 1 {
                            prefix.extend([
                                Instr::ImplPrim(ImplPrimitive::UnBox, span),
                                Instr::PopTemp {
                                    stack: TempStack::Inline,
                                    count: 1,
                                    span,
                                },
                            ]);
                        }
                    }
                    prefix.push(Instr::ImplPrim(ImplPrimitive::UnBox, span));
                }
                prefix.extend(instrs);
                finish!(prefix, sig);
            }
            Stringify => {
                let operand = modified.code_operands().next().unwrap();
                let s = format_word(operand, &self.asm.inputs);
                let instr = Instr::Push(s.into());
                finish!([instr], Signature::new(0, 1));
            }
            Quote => {
                let operand = modified.code_operands().next().unwrap().clone();
                self.new_functions.push(EcoVec::new());
                self.do_comptime(prim, operand, &modified.modifier.span, true)?;
                let instrs = self.new_functions.pop().unwrap();
                let code: String = match instrs.as_slice() {
                    [Instr::Push(Value::Char(chars))] if chars.rank() == 1 => {
                        chars.data.iter().collect()
                    }
                    [Instr::Push(Value::Char(chars))] => {
                        return Err(self.fatal_error(
                            modified.modifier.span.clone(),
                            format!(
                                "quote's argument compiled to a \
                                rank {} array rather than a string",
                                chars.rank()
                            ),
                        ))
                    }
                    [Instr::Push(value)] => {
                        return Err(self.fatal_error(
                            modified.modifier.span.clone(),
                            format!(
                                "quote's argument compiled to a \
                                {} array rather than a string",
                                value.type_name()
                            ),
                        ))
                    }
                    _ => {
                        return Err(self.fatal_error(
                            modified.modifier.span.clone(),
                            "quote's argument did not compile to a string",
                        ));
                    }
                };
                self.quote(&code, &modified.modifier.span, call)?;
            }
            Sig => {
                let operand = modified.code_operands().next().unwrap().clone();
                let (_, sig) = self.compile_operand_word(operand)?;
                let instrs = [
                    Instr::Push(sig.outputs.into()),
                    Instr::Push(sig.args.into()),
                ];
                finish!(instrs, Signature::new(0, 2));
            }
            _ => return Ok(false),
        }
        self.handle_primitive_experimental(prim, &modified.modifier.span);
        self.handle_primitive_deprecation(prim, &modified.modifier.span);
        Ok(true)
    }
    /// Expand a stack macro
    fn expand_macro(
        &mut self,
        name: Ident,
        macro_words: &mut Vec<Sp<Word>>,
        mut operands: Vec<Sp<Word>>,
        span: CodeSpan,
    ) -> UiuaResult {
        // Mark the operands as macro arguments
        set_in_macro_arg(&mut operands);
        // Collect placeholders
        let mut ops = collect_placeholder(macro_words);
        ops.reverse();
        let span = span.merge(operands.last().unwrap().span.clone());
        // Initialize the placeholder stack
        let mut ph_stack: Vec<Sp<Word>> =
            operands.into_iter().filter(|w| w.value.is_code()).collect();
        let mut replaced = Vec::new();
        // Run the placeholder operations
        for op in ops {
            let span = op.span;
            let op = op.value;
            let mut pop = || {
                (ph_stack.pop())
                    .ok_or_else(|| self.fatal_error(span.clone(), "Operand stack is empty"))
            };
            match op {
                PlaceholderOp::Call => replaced.push(pop()?),
                PlaceholderOp::Dup => {
                    let a = pop()?;
                    ph_stack.push(a.clone());
                    ph_stack.push(a);
                }
                PlaceholderOp::Flip => {
                    let a = pop()?;
                    let b = pop()?;
                    ph_stack.push(a);
                    ph_stack.push(b);
                }
                PlaceholderOp::Over => {
                    let a = pop()?;
                    let b = pop()?;
                    ph_stack.push(b.clone());
                    ph_stack.push(a);
                    ph_stack.push(b);
                }
            }
        }
        // Warn if there are operands left
        if !ph_stack.is_empty() {
            let span = (ph_stack.first().unwrap().span.clone())
                .merge(ph_stack.last().unwrap().span.clone());
            self.emit_diagnostic(
                format!(
                    "Macro operand stack has {} item{} left",
                    ph_stack.len(),
                    if ph_stack.len() == 1 { "" } else { "s" }
                ),
                DiagnosticKind::Warning,
                span,
            );
        }
        // Replace placeholders in the macro's words
        let mut operands = replaced.into_iter().rev();
        replace_placeholders(macro_words, &mut || operands.next().unwrap());
        // Format and store the expansion for the LSP
        let mut words_to_format = Vec::new();
        for word in &*macro_words {
            match &word.value {
                Word::Func(func) => words_to_format.extend(func.lines.iter().flatten().cloned()),
                _ => words_to_format.push(word.clone()),
            }
        }
        let formatted = format_words(&words_to_format, &self.asm.inputs);
        (self.code_meta.macro_expansions).insert(span, (name, formatted));
        Ok(())
    }
    fn quote(&mut self, code: &str, span: &CodeSpan, call: bool) -> UiuaResult {
        let (items, errors, _) = parse(
            code,
            InputSrc::Macro(span.clone().into()),
            &mut self.asm.inputs,
        );
        if !errors.is_empty() {
            return Err(
                UiuaError::Parse(errors, self.asm.inputs.clone().into()).trace_macro(span.clone())
            );
        }

        // Compile the generated items
        for item in items {
            match item {
                Item::Words(words) => {
                    for line in words {
                        self.words(line, call)
                            .map_err(|e| e.trace_macro(span.clone()))?;
                    }
                }
                Item::Binding(binding) => self
                    .binding(binding, None)
                    .map_err(|e| e.trace_macro(span.clone()))?,
                Item::Import(import) => self
                    .import(import, None)
                    .map_err(|e| e.trace_macro(span.clone()))?,
                Item::TestScope(_) => {
                    self.add_error(span.clone(), "Macros may not generate test scopes")
                }
            };
        }

        Ok(())
    }
    fn do_comptime(
        &mut self,
        prim: Primitive,
        operand: Sp<Word>,
        span: &CodeSpan,
        call: bool,
    ) -> UiuaResult {
        if self.pre_eval_mode == PreEvalMode::Lsp {
            return self.word(operand, call);
        }
        let mut comp = self.clone();
        let (instrs, sig) = comp.compile_operand_word(operand)?;
        if sig.args > 0 {
            return Err(self.fatal_error(
                span.clone(),
                format!(
                    "{}'s function must have no arguments, but it has {}",
                    prim.format(),
                    sig.args
                ),
            ));
        }
        let instrs = optimize_instrs(instrs, true, &comp);
        let start = comp.asm.instrs.len();
        let len = instrs.len();
        comp.asm.instrs.extend(instrs);
        comp.asm.top_slices.push(FuncSlice { start, len });
        comp.prepare_env()?;
        let values = match comp.macro_env.run_asm(&comp.asm) {
            Ok(_) => comp.macro_env.take_stack(),
            Err(e) => {
                if self.errors.is_empty() {
                    self.add_error(span.clone(), format!("Compile-time evaluation failed: {e}"));
                }
                vec![Value::default(); sig.outputs]
            }
        };
        if !call {
            self.new_functions.push(EcoVec::new());
        }
        let val_count = sig.outputs;
        for value in values.into_iter().rev().take(val_count).rev() {
            self.push_instr(Instr::push(value));
        }
        if !call {
            let instrs = self.new_functions.pop().unwrap();
            let sig = Signature::new(0, val_count);
            let func = self.make_function(FunctionId::Anonymous(span.clone()), sig, instrs);
            self.push_instr(Instr::PushFunc(func));
        }
        Ok(())
    }
    /// Prepare the macro environment to run some expanded code.
    pub(super) fn prepare_env(&mut self) -> UiuaResult {
        let top_slices = take(&mut self.macro_env.asm.top_slices);
        let mut bindings = take(&mut self.macro_env.asm.bindings);
        bindings.extend_from_slice(&self.asm.bindings[bindings.len()..]);
        self.macro_env.asm = self.asm.clone();
        self.macro_env.asm.bindings = bindings;
        if let Some(last_slice) = top_slices.last() {
            (self.macro_env.asm.top_slices).retain(|slice| slice.start > last_slice.start);
        }
        self.macro_env.no_io(Uiua::run_top_slices)?;
        Ok(())
    }
    /// Run a function in a temporary scope with the given names.
    /// Newly created bindings will be added to the current scope after the function is run.
    fn temp_scope<T>(
        &mut self,
        names: IndexMap<Ident, LocalName>,
        f: impl FnOnce(&mut Self) -> T,
    ) -> T {
        let macro_names_len = names.len();
        let temp_scope = Scope {
            kind: ScopeKind::Temp,
            names,
            experimental: self.scope.experimental,
            ..Default::default()
        };
        self.higher_scopes
            .push(replace(&mut self.scope, temp_scope));
        let res = f(self);
        let mut scope = self.higher_scopes.pop().unwrap();
        (scope.names).extend(self.scope.names.drain(macro_names_len..));
        self.scope = scope;
        res
    }
}

fn instrs_have_pattern_matching(instrs: &[Instr], asm: &Assembly) -> bool {
    use ImplPrimitive::*;
    use Primitive::*;
    // Collect positions of `try` instructions
    let try_pos: BTreeSet<usize> = instrs
        .iter()
        .enumerate()
        .filter(|(_, instr)| matches!(instr, Instr::Prim(Try, _) | Instr::ImplPrim(TryPattern, _)))
        .map(|(i, _)| i)
        .collect();
    instrs.iter().enumerate().any(|(i, instr)| match instr {
        // Explicit pattern matching
        Instr::ImplPrim(MatchPattern | UnJoinPattern, _) | Instr::MatchFormatPattern { .. } => true,
        // Non-`try` functions
        Instr::PushFunc(f) if !try_pos.contains(&(i + 1)) => {
            instrs_have_pattern_matching(f.instrs(asm), asm)
        }
        _ => false,
    })
}
