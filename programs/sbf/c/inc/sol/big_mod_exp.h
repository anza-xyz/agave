#pragma once
/**
 * @brief Solana big_mod_exp system call
**/

#ifdef __cplusplus
extern "C" {
#endif

#define BIG_MOD_EXP_ENDIANNESS_BE 0
#define BIG_MOD_EXP_ENDIANNESS_LE 1
#define BIG_MOD_EXP_MAX_BYTES 512

typedef struct {
  uint64_t base_addr;
  uint64_t base_len;
  uint64_t exponent_addr;
  uint64_t exponent_len;
  uint64_t modulus_addr;
  uint64_t modulus_len;
  uint64_t result_addr;
  uint64_t result_len;
} SolBigModExpParams;

/**
 * Big integer modular exponentiation
 *
 * @param endianness BIG_MOD_EXP_ENDIANNESS_BE or BIG_MOD_EXP_ENDIANNESS_LE
 * @param params Pointer to SolBigModExpParams struct
 * @return 0 if executed successfully
 */
/* DO NOT MODIFY THIS GENERATED FILE. INSTEAD CHANGE platform-tools-sdk/sbf/c/inc/sol/inc/big_mod_exp.inc AND RUN `cargo run --bin gen-headers` */
#ifndef SOL_SBPFV3
uint64_t sol_big_mod_exp(uint64_t, const SolBigModExpParams *);
#else
typedef uint64_t(*sol_big_mod_exp_pointer_type)(uint64_t, const SolBigModExpParams *);
static uint64_t sol_big_mod_exp(uint64_t arg1, const SolBigModExpParams * arg2) {
  sol_big_mod_exp_pointer_type sol_big_mod_exp_pointer = (sol_big_mod_exp_pointer_type) 2014202901;
  return sol_big_mod_exp_pointer(arg1, arg2);
}
#endif

#ifdef __cplusplus
}
#endif

/**@}*/
