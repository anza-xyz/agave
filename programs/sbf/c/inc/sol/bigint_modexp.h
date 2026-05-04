#pragma once
/**
 * @brief Solana bigint_modexp system call
**/

#ifdef __cplusplus
extern "C" {
#endif

#define BIGINT_ENDIANNESS_BE 0
#define BIGINT_ENDIANNESS_LE 1
#define BIGINT_MODEXP_MAX_BYTES 512

typedef struct {
  uint64_t base_addr;
  uint64_t base_len;
  uint64_t exponent_addr;
  uint64_t exponent_len;
  uint64_t modulus_addr;
  uint64_t modulus_len;
  uint64_t result_addr;
  uint64_t result_len;
} SolBigIntModExpParams;

/**
 * Big integer modular exponentiation
 *
 * @param endianness BIGINT_ENDIANNESS_BE or BIGINT_ENDIANNESS_LE
 * @param params Pointer to SolBigIntModExpParams struct
 * @return 0 if executed successfully
 */
/* DO NOT MODIFY THIS GENERATED FILE. INSTEAD CHANGE platform-tools-sdk/sbf/c/inc/sol/inc/bigint_modexp.inc AND RUN `cargo run --bin gen-headers` */
#ifndef SOL_SBPFV3
uint64_t sol_bigint_modexp(uint64_t, const SolBigIntModExpParams *);
#else
typedef uint64_t(*sol_bigint_modexp_pointer_type)(uint64_t, const SolBigIntModExpParams *);
static uint64_t sol_bigint_modexp(uint64_t arg1, const SolBigIntModExpParams * arg2) {
  sol_bigint_modexp_pointer_type sol_bigint_modexp_pointer = (sol_bigint_modexp_pointer_type) 689211112;
  return sol_bigint_modexp_pointer(arg1, arg2);
}
#endif

#ifdef __cplusplus
}
#endif

/**@}*/
