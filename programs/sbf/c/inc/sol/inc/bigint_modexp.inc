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
@SYSCALL uint64_t sol_bigint_modexp(uint64_t, const SolBigIntModExpParams *);

#ifdef __cplusplus
}
#endif

/**@}*/
