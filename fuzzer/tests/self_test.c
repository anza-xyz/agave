#include <assert.h>
#include <dlfcn.h>
#include <stdio.h>

int main(int argc, char **argv) {
    if (argc != 2) {
        fprintf(stderr, "Usage: %s <lib_path>\n", argv[0]);
        return 1;
    }
    char *lib_path = argv[1];
    void *handle = dlopen(lib_path, RTLD_NOW);
    assert(handle);
    assert(dlsym(handle, "sol_compat_init"));
    assert(dlsym(handle, "sol_compat_fini"));
    assert(dlsym(handle, "sol_compat_get_features_v1"));
    assert(dlsym(handle, "sol_compat_instr_execute_v1"));
    dlclose(handle);
    fputs("OK\n", stderr);
    return 0;
}
