#pragma once
/**
 * @brief Solana big_mod_exp system call
**/

#ifdef __cplusplus
extern "C" {
#endif

#define BIG_MOD_EXP_MAX_BYTES 512

typedef struct {
  const uint8_t *base;
  uint64_t base_len;
  const uint8_t *exponent;
  uint64_t exponent_len;
  const uint8_t *modulus;
  uint64_t modulus_len;
} SolBigModExpParams;

/**
 * Big integer modular exponentiation
 *
 * @param params Pointer to SolBigModExpParams bytes
 * @param result Pointer to writable result buffer, at least params->modulus_len bytes
 * @return 0 if executed successfully
 */
/* DO NOT MODIFY THIS GENERATED FILE. INSTEAD CHANGE platform-tools-sdk/sbf/c/inc/sol/inc/big_mod_exp.inc AND RUN `cargo run --bin gen-headers` */
#ifndef SOL_SBPFV3
uint64_t sol_big_mod_exp(const uint8_t *, uint8_t *);
#else
typedef uint64_t(*sol_big_mod_exp_pointer_type)(const uint8_t *, uint8_t *);
static uint64_t sol_big_mod_exp(const uint8_t * arg1, uint8_t * arg2) {
  sol_big_mod_exp_pointer_type sol_big_mod_exp_pointer = (sol_big_mod_exp_pointer_type) 2014202901;
  return sol_big_mod_exp_pointer(arg1, arg2);
}
#endif

#ifdef __cplusplus
}
#endif

/**@}*/
