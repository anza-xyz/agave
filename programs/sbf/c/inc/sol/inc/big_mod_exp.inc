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
@SYSCALL uint64_t sol_big_mod_exp(const uint8_t *, uint8_t *);

#ifdef __cplusplus
}
#endif

/**@}*/
