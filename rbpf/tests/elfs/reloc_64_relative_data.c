/**
 * @brief a program to test R_BPF_64_RELATIVE relocation handling
 */

typedef unsigned long int uint64_t;
typedef unsigned char uint8_t;

// this will store __FILE__ and generate a relocation for FILE to refer to it
volatile const uint64_t FILE = (uint64_t) __FILE__;

extern uint64_t entrypoint(const uint8_t *input) {
  return FILE;
}
