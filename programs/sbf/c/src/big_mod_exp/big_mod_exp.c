/**
 * @brief Big integer modular exponentiation Syscall test
 */

#include <solana_sdk.h>

extern uint64_t entrypoint(const uint8_t *input) {

    SolBigModExpParams params;

    uint8_t base[] = { 0x05 };
    uint8_t exponent[] = { 0x02 };
    uint8_t modulus[] = { 0x07 };
    uint8_t expected[] = { 0x04 };
    uint8_t result[sizeof(modulus)];

    params.base = base;
    params.base_len = sizeof(base);
    params.exponent = exponent;
    params.exponent_len = sizeof(exponent);
    params.modulus = modulus;
    params.modulus_len = sizeof(modulus);

    uint64_t result_code = sol_big_mod_exp((const uint8_t *)&params, result);

    sol_assert(0 == result_code);
    sol_assert(0 == sol_memcmp(result, expected, sizeof(expected)));

    return SUCCESS;
}
