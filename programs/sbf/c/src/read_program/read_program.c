#include <solana_sdk.h>

extern uint64_t entrypoint(const uint8_t *input) {
  SolAccountInfo ka[1];
  SolParameters params = (SolParameters){.ka = ka};

  sol_log(__FILE__);

  if (!sol_deserialize(input, &params, SOL_ARRAY_SIZE(ka))) {
    return ERROR_INVALID_ARGUMENT;
  }

  sol_assert(params.ka_num == 1);
  sol_assert(!sol_memcmp(params.ka[0].data, params.data, params.data_len));
  sol_assert(params.ka[0].is_signer == false);
  sol_assert(params.ka[0].is_writable == false);
  sol_assert(params.ka[0].executable == true);

  return SUCCESS;
}
