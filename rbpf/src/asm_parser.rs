#![allow(clippy::arithmetic_side_effects)]
// Copyright 2017 Rich Lane <lanerl@gmail.com>
//
// Licensed under the Apache License, Version 2.0 <http://www.apache.org/licenses/LICENSE-2.0> or
// the MIT license <http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

// Rust-doc comments were left in the module, but it is no longer publicly exposed from the root
// file of the crate. Do not expect to find those comments in the documentation of the crate.

//! This module parses eBPF assembly language source code.

use combine::{
    attempt, between,
    char::{alpha_num, char, digit, hex_digit, spaces, string},
    combine_parse_partial, combine_parser_impl,
    easy::{Error, Errors, Info},
    eof, many, many1, one_of, optional, parse_mode, parser, sep_by, skip_many,
    stream::state::{SourcePosition, State},
    Parser, Stream,
};

/// Operand of an instruction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Operand {
    /// Register number.
    Register(i64),
    /// Jump offset or immediate.
    Integer(i64),
    /// Register number and offset.
    Memory(i64, i64),
    /// Jump target label.
    Label(String),
}

/// Parsed statement.
#[derive(Debug, PartialEq, Eq)]
pub enum Statement {
    /// Parsed label (name).
    Label { name: String },
    /// Parsed directive (name, operands).
    Directive {
        name: String,
        operands: Vec<Operand>,
    },
    /// Parsed instruction (name, operands).
    Instruction {
        name: String,
        operands: Vec<Operand>,
    },
}

parser! {
    fn ident[I]()(I) -> String where [I: Stream<Item=char>] {
        many1(alpha_num().or(char('_')))
    }
}

parser! {
    fn mnemonic[I]()(I) -> String where [I: Stream<Item=char>] {
        many1(alpha_num())
    }
}

parser! {
    fn integer[I]()(I) -> i64 where [I: Stream<Item=char>] {
        let sign = optional(one_of("-+".chars()).skip(skip_many(char(' ')))).map(|x| match x {
            Some('-') => -1,
            _ => 1,
        });
        let hex = string("0x")
            .with(many1(hex_digit()))
            .map(|x: String| u64::from_str_radix(&x, 16).unwrap_or(0) as i64);
        let dec = many1(digit()).map(|x: String| x.parse::<i64>().unwrap_or(0));
        (sign, attempt(hex).or(dec))
            .map(|(s, x)| s * x)
    }
}

parser! {
    fn register[I]()(I) -> i64 where [I: Stream<Item=char>] {
        char('r')
            .with(many1(digit()))
            .map(|x: String| x.parse::<i64>().unwrap_or(0))
    }
}

parser! {
    fn operand[I]()(I) -> Operand where [I: Stream<Item=char>] {
        let register_operand = register().map(Operand::Register);
        let immediate = integer().map(Operand::Integer);
        let memory = between(
            char('['),
            char(']'),
            (register().skip(skip_many(char(' '))), optional(integer())),
        )
        .map(|t| Operand::Memory(t.0, t.1.unwrap_or(0)));
        let label = ident().map(Operand::Label);
        register_operand
            .or(immediate)
            .or(memory)
            .or(label)
    }
}

parser! {
    fn label[I]()(I) -> Statement where [I: Stream<Item=char>] {
        (ident(), char(':'))
            .map(|t| Statement::Label { name: t.0 })
    }
}

parser! {
    fn directive[I]()(I) -> Statement where [I: Stream<Item=char>] {
        let operands = sep_by(operand(), char(',').skip(skip_many(char(' '))));
        (char('.').with(many1(alpha_num())).skip(skip_many(char(' '))), operands)
            .map(|t| Statement::Directive { name: t.0, operands: t.1 })
    }
}

parser! {
    fn instruction[I]()(I) -> Statement where [I: Stream<Item=char>] {
        let operands = sep_by(operand(), char(',').skip(skip_many(char(' '))));
        (mnemonic().skip(skip_many(char(' '))), operands)
            .map(|t| Statement::Instruction { name: t.0, operands: t.1 })
    }
}

fn format_info(info: &Info<char, &str>) -> String {
    match *info {
        Info::Token(x) => format!("{x:?}"),
        Info::Range(x) => format!("{x:?}"),
        Info::Owned(ref x) => x.clone(),
        Info::Borrowed(x) => x.to_string(),
    }
}

fn format_error(error: &Error<char, &str>) -> String {
    match *error {
        Error::Unexpected(ref x) => format!("unexpected {}", format_info(x)),
        Error::Expected(ref x) => format!("expected {}", format_info(x)),
        Error::Message(ref x) => format_info(x),
        Error::Other(ref x) => format!("{x:?}"),
    }
}

fn format_parse_error(parse_error: &Errors<char, &str, SourcePosition>) -> String {
    format!(
        "Parse error at line {} column {}: {}",
        parse_error.position.line,
        parse_error.position.column,
        parse_error
            .errors
            .iter()
            .map(format_error)
            .collect::<Vec<String>>()
            .join(", ")
    )
}

/// Parse a string into a list of instructions.
///
/// The instructions are not validated and may have invalid names and operand types.
pub fn parse(input: &str) -> Result<Vec<Statement>, String> {
    match spaces()
        .with(many(
            attempt(label())
                .or(directive())
                .or(instruction())
                .skip(spaces()),
        ))
        .skip(eof())
        .easy_parse(State::with_positioner(input, SourcePosition::default()))
    {
        Ok((insts, _)) => Ok(insts),
        Err(err) => Err(format_parse_error(&err)),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ident, instruction, integer, mnemonic, operand, parse, register, Operand, Statement,
    };
    use combine::Parser;

    // Unit tests for the different kinds of parsers.

    #[test]
    fn test_ident() {
        assert_eq!(
            ident().parse("entrypoint"),
            Ok(("entrypoint".to_string(), ""))
        );
        assert_eq!(ident().parse("lbb_1"), Ok(("lbb_1".to_string(), "")));
        assert_eq!(ident().parse("exit:"), Ok(("exit".to_string(), ":")));
    }

    #[test]
    fn test_mnemonic() {
        assert_eq!(mnemonic().parse("nop"), Ok(("nop".to_string(), "")));
        assert_eq!(mnemonic().parse("add32"), Ok(("add32".to_string(), "")));
        assert_eq!(mnemonic().parse("add32*"), Ok(("add32".to_string(), "*")));
    }

    #[test]
    fn test_integer() {
        assert_eq!(integer().parse("0"), Ok((0, "")));
        assert_eq!(integer().parse("42"), Ok((42, "")));
        assert_eq!(integer().parse("+42"), Ok((42, "")));
        assert_eq!(integer().parse("-42"), Ok((-42, "")));
        assert_eq!(integer().parse("0x0"), Ok((0, "")));
        assert_eq!(
            integer().parse("0x123456789abcdef0"),
            Ok((0x123456789abcdef0, ""))
        );
        assert_eq!(integer().parse("-0x1f"), Ok((-31, "")));
    }

    #[test]
    fn test_register() {
        assert_eq!(register().parse("r0"), Ok((0, "")));
        assert_eq!(register().parse("r15"), Ok((15, "")));
    }

    #[test]
    fn test_operand() {
        assert_eq!(operand().parse("r0"), Ok((Operand::Register(0), "")));
        assert_eq!(operand().parse("r15"), Ok((Operand::Register(15), "")));
        assert_eq!(operand().parse("0"), Ok((Operand::Integer(0), "")));
        assert_eq!(operand().parse("42"), Ok((Operand::Integer(42), "")));
        assert_eq!(operand().parse("[r1]"), Ok((Operand::Memory(1, 0), "")));
        assert_eq!(operand().parse("[r3+5]"), Ok((Operand::Memory(3, 5), "")));
        assert_eq!(
            operand().parse("[r3+0x1f]"),
            Ok((Operand::Memory(3, 31), ""))
        );
        assert_eq!(
            operand().parse("[r3-0x1f]"),
            Ok((Operand::Memory(3, -31), ""))
        );
        assert_eq!(operand().parse("[r5 + 3]"), Ok((Operand::Memory(5, 3), "")));
        assert_eq!(
            operand().parse("[r11 - 0x30]"),
            Ok((Operand::Memory(11, -48), ""))
        );
    }

    #[test]
    fn test_instruction() {
        assert_eq!(
            instruction().parse("exit"),
            Ok((
                Statement::Instruction {
                    name: "exit".to_string(),
                    operands: vec![],
                },
                ""
            ))
        );

        assert_eq!(
            instruction().parse("call 2"),
            Ok((
                Statement::Instruction {
                    name: "call".to_string(),
                    operands: vec![Operand::Integer(2)],
                },
                ""
            ))
        );

        assert_eq!(
            instruction().parse("addi r1, 2"),
            Ok((
                Statement::Instruction {
                    name: "addi".to_string(),
                    operands: vec![Operand::Register(1), Operand::Integer(2)],
                },
                ""
            ))
        );

        assert_eq!(
            instruction().parse("ldxb r2, [r1+12]"),
            Ok((
                Statement::Instruction {
                    name: "ldxb".to_string(),
                    operands: vec![Operand::Register(2), Operand::Memory(1, 12)],
                },
                ""
            ))
        );

        assert_eq!(
            instruction().parse("lsh r3, 0x8"),
            Ok((
                Statement::Instruction {
                    name: "lsh".to_string(),
                    operands: vec![Operand::Register(3), Operand::Integer(8)],
                },
                ""
            ))
        );

        assert_eq!(
            instruction().parse("jne r3, 0x8, +37"),
            Ok((
                Statement::Instruction {
                    name: "jne".to_string(),
                    operands: vec![
                        Operand::Register(3),
                        Operand::Integer(8),
                        Operand::Integer(37)
                    ],
                },
                ""
            ))
        );

        // Whitespace between operands is optional.
        assert_eq!(
            instruction().parse("jne r3,0x8,+37"),
            Ok((
                Statement::Instruction {
                    name: "jne".to_string(),
                    operands: vec![
                        Operand::Register(3),
                        Operand::Integer(8),
                        Operand::Integer(37)
                    ],
                },
                ""
            ))
        );
    }

    // Other unit tests: try to parse various set of instructions.

    #[test]
    fn test_empty() {
        assert_eq!(parse(""), Ok(vec![]));
    }

    #[test]
    fn test_exit() {
        // No operands.
        assert_eq!(
            parse("exit"),
            Ok(vec![Statement::Instruction {
                name: "exit".to_string(),
                operands: vec![],
            }])
        );
    }

    #[test]
    fn test_lsh() {
        // Register and immediate operands.
        assert_eq!(
            parse("lsh r3, 0x20"),
            Ok(vec![Statement::Instruction {
                name: "lsh".to_string(),
                operands: vec![Operand::Register(3), Operand::Integer(0x20)],
            }])
        );
    }

    #[test]
    fn test_ja() {
        // Jump offset operand.
        assert_eq!(
            parse("ja +1"),
            Ok(vec![Statement::Instruction {
                name: "ja".to_string(),
                operands: vec![Operand::Integer(1)],
            }])
        );
    }

    #[test]
    fn test_ldxh() {
        // Register and memory operands.
        assert_eq!(
            parse("ldxh r4, [r1+12]"),
            Ok(vec![Statement::Instruction {
                name: "ldxh".to_string(),
                operands: vec![Operand::Register(4), Operand::Memory(1, 12)],
            }])
        );
    }

    #[test]
    fn test_tcp_sack() {
        // Sample program from ubpf.
        // We could technically indent the instructions since the parser support white spaces at
        // the beginning, but there is another test for that.
        let src = "\
ldxb r2, [r1+12]
ldxb r3, [r1+13]
lsh r3, 0x8
or r3, r2
mov r0, 0x0
jne r3, 0x8, +37
ldxb r2, [r1+23]
jne r2, 0x6, +35
ldxb r2, [r1+14]
add r1, 0xe
and r2, 0xf
lsh r2, 0x2
add r1, r2
mov r0, 0x0
ldxh r4, [r1+12]
add r1, 0x14
rsh r4, 0x2
and r4, 0x3c
mov r2, r4
add r2, 0xffffffec
mov r5, 0x15
mov r3, 0x0
jgt r5, r4, +20
mov r5, r3
lsh r5, 0x20
arsh r5, 0x20
mov r4, r1
add r4, r5
ldxb r5, [r4]
jeq r5, 0x1, +4
jeq r5, 0x0, +12
mov r6, r3
jeq r5, 0x5, +9
ja +2
add r3, 0x1
mov r6, r3
ldxb r3, [r4+1]
add r3, r6
lsh r3, 0x20
arsh r3, 0x20
jsgt r2, r3, -18
ja +1
mov r0, 0x1
exit
";

        assert_eq!(
            parse(src),
            Ok(vec![
                Statement::Instruction {
                    name: "ldxb".to_string(),
                    operands: vec![Operand::Register(2), Operand::Memory(1, 12)],
                },
                Statement::Instruction {
                    name: "ldxb".to_string(),
                    operands: vec![Operand::Register(3), Operand::Memory(1, 13)],
                },
                Statement::Instruction {
                    name: "lsh".to_string(),
                    operands: vec![Operand::Register(3), Operand::Integer(8)],
                },
                Statement::Instruction {
                    name: "or".to_string(),
                    operands: vec![Operand::Register(3), Operand::Register(2)],
                },
                Statement::Instruction {
                    name: "mov".to_string(),
                    operands: vec![Operand::Register(0), Operand::Integer(0)],
                },
                Statement::Instruction {
                    name: "jne".to_string(),
                    operands: vec![
                        Operand::Register(3),
                        Operand::Integer(8),
                        Operand::Integer(37)
                    ],
                },
                Statement::Instruction {
                    name: "ldxb".to_string(),
                    operands: vec![Operand::Register(2), Operand::Memory(1, 23)],
                },
                Statement::Instruction {
                    name: "jne".to_string(),
                    operands: vec![
                        Operand::Register(2),
                        Operand::Integer(6),
                        Operand::Integer(35)
                    ],
                },
                Statement::Instruction {
                    name: "ldxb".to_string(),
                    operands: vec![Operand::Register(2), Operand::Memory(1, 14)],
                },
                Statement::Instruction {
                    name: "add".to_string(),
                    operands: vec![Operand::Register(1), Operand::Integer(14)],
                },
                Statement::Instruction {
                    name: "and".to_string(),
                    operands: vec![Operand::Register(2), Operand::Integer(15)],
                },
                Statement::Instruction {
                    name: "lsh".to_string(),
                    operands: vec![Operand::Register(2), Operand::Integer(2)],
                },
                Statement::Instruction {
                    name: "add".to_string(),
                    operands: vec![Operand::Register(1), Operand::Register(2)],
                },
                Statement::Instruction {
                    name: "mov".to_string(),
                    operands: vec![Operand::Register(0), Operand::Integer(0)],
                },
                Statement::Instruction {
                    name: "ldxh".to_string(),
                    operands: vec![Operand::Register(4), Operand::Memory(1, 12)],
                },
                Statement::Instruction {
                    name: "add".to_string(),
                    operands: vec![Operand::Register(1), Operand::Integer(20)],
                },
                Statement::Instruction {
                    name: "rsh".to_string(),
                    operands: vec![Operand::Register(4), Operand::Integer(2)],
                },
                Statement::Instruction {
                    name: "and".to_string(),
                    operands: vec![Operand::Register(4), Operand::Integer(60)],
                },
                Statement::Instruction {
                    name: "mov".to_string(),
                    operands: vec![Operand::Register(2), Operand::Register(4)],
                },
                Statement::Instruction {
                    name: "add".to_string(),
                    operands: vec![Operand::Register(2), Operand::Integer(4294967276)],
                },
                Statement::Instruction {
                    name: "mov".to_string(),
                    operands: vec![Operand::Register(5), Operand::Integer(21)],
                },
                Statement::Instruction {
                    name: "mov".to_string(),
                    operands: vec![Operand::Register(3), Operand::Integer(0)],
                },
                Statement::Instruction {
                    name: "jgt".to_string(),
                    operands: vec![
                        Operand::Register(5),
                        Operand::Register(4),
                        Operand::Integer(20)
                    ],
                },
                Statement::Instruction {
                    name: "mov".to_string(),
                    operands: vec![Operand::Register(5), Operand::Register(3)],
                },
                Statement::Instruction {
                    name: "lsh".to_string(),
                    operands: vec![Operand::Register(5), Operand::Integer(32)],
                },
                Statement::Instruction {
                    name: "arsh".to_string(),
                    operands: vec![Operand::Register(5), Operand::Integer(32)],
                },
                Statement::Instruction {
                    name: "mov".to_string(),
                    operands: vec![Operand::Register(4), Operand::Register(1)],
                },
                Statement::Instruction {
                    name: "add".to_string(),
                    operands: vec![Operand::Register(4), Operand::Register(5)],
                },
                Statement::Instruction {
                    name: "ldxb".to_string(),
                    operands: vec![Operand::Register(5), Operand::Memory(4, 0)],
                },
                Statement::Instruction {
                    name: "jeq".to_string(),
                    operands: vec![
                        Operand::Register(5),
                        Operand::Integer(1),
                        Operand::Integer(4)
                    ],
                },
                Statement::Instruction {
                    name: "jeq".to_string(),
                    operands: vec![
                        Operand::Register(5),
                        Operand::Integer(0),
                        Operand::Integer(12)
                    ],
                },
                Statement::Instruction {
                    name: "mov".to_string(),
                    operands: vec![Operand::Register(6), Operand::Register(3)],
                },
                Statement::Instruction {
                    name: "jeq".to_string(),
                    operands: vec![
                        Operand::Register(5),
                        Operand::Integer(5),
                        Operand::Integer(9)
                    ],
                },
                Statement::Instruction {
                    name: "ja".to_string(),
                    operands: vec![Operand::Integer(2)],
                },
                Statement::Instruction {
                    name: "add".to_string(),
                    operands: vec![Operand::Register(3), Operand::Integer(1)],
                },
                Statement::Instruction {
                    name: "mov".to_string(),
                    operands: vec![Operand::Register(6), Operand::Register(3)],
                },
                Statement::Instruction {
                    name: "ldxb".to_string(),
                    operands: vec![Operand::Register(3), Operand::Memory(4, 1)],
                },
                Statement::Instruction {
                    name: "add".to_string(),
                    operands: vec![Operand::Register(3), Operand::Register(6)],
                },
                Statement::Instruction {
                    name: "lsh".to_string(),
                    operands: vec![Operand::Register(3), Operand::Integer(32)],
                },
                Statement::Instruction {
                    name: "arsh".to_string(),
                    operands: vec![Operand::Register(3), Operand::Integer(32)],
                },
                Statement::Instruction {
                    name: "jsgt".to_string(),
                    operands: vec![
                        Operand::Register(2),
                        Operand::Register(3),
                        Operand::Integer(-18)
                    ],
                },
                Statement::Instruction {
                    name: "ja".to_string(),
                    operands: vec![Operand::Integer(1)],
                },
                Statement::Instruction {
                    name: "mov".to_string(),
                    operands: vec![Operand::Register(0), Operand::Integer(1)],
                },
                Statement::Instruction {
                    name: "exit".to_string(),
                    operands: vec![],
                }
            ])
        );
    }

    #[test]
    fn test_error_eof() {
        // Unexpected end of input in a register name.
        assert_eq!(
            parse("lsh r"),
            Err(
                "Parse error at line 1 column 6: unexpected end of input, expected digit"
                    .to_string()
            )
        );
    }

    #[test]
    fn test_error_unexpected_character() {
        // Unexpected character at end of input.
        assert_eq!(
            parse("exit\n^"),
            Err(
                "Parse error at line 2 column 1: unexpected '^', expected letter or digit, expected '_', expected '.', expected whitespaces, expected end of input".to_string()
            )
        );
    }

    #[test]
    fn test_initial_whitespace() {
        assert_eq!(
            parse(
                "
                          exit"
            ),
            Ok(vec![Statement::Instruction {
                name: "exit".to_string(),
                operands: vec![],
            }])
        );
    }
}
