#include <stdint.h>
#include <time.h>

#if defined(__GNUC__) || defined(__clang__)
#define STSPY_NOINLINE __attribute__((noinline, used, visibility("default")))
#else
#define STSPY_NOINLINE
#endif

void *interpreterProxy = 0;
volatile uint64_t stspy_deep_native_sink = 0;

char *getModuleName(void) {
    return "STSpyDeepNativePlugin";
}

int setInterpreter(void *proxy) {
    interpreterProxy = proxy;
    return 1;
}

STSPY_NOINLINE uint64_t stspy_native_leaf(uint64_t state, int iterations) {
    struct timespec ts;

    for (int i = 0; i < iterations; i++) {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state += (uint64_t)i * 0x9e3779b97f4a7c15ULL;

        if ((i & 0x3fff) == 0) {
            clock_gettime(CLOCK_MONOTONIC, &ts);
            state ^= (uint64_t)ts.tv_nsec;
        }
    }

    stspy_deep_native_sink = state;
    return state;
}

STSPY_NOINLINE uint64_t stspy_native_level_8(uint64_t state) {
    uint64_t result = stspy_native_leaf(state + 8, 220000);
    stspy_deep_native_sink ^= result + state;
    return result ^ 8;
}

STSPY_NOINLINE uint64_t stspy_native_level_7(uint64_t state) {
    uint64_t result = stspy_native_level_8(state + 7);
    stspy_deep_native_sink ^= result + state;
    return result ^ 7;
}

STSPY_NOINLINE uint64_t stspy_native_level_6(uint64_t state) {
    uint64_t result = stspy_native_level_7(state + 6);
    stspy_deep_native_sink ^= result + state;
    return result ^ 6;
}

STSPY_NOINLINE uint64_t stspy_native_level_5(uint64_t state) {
    uint64_t result = stspy_native_level_6(state + 5);
    stspy_deep_native_sink ^= result + state;
    return result ^ 5;
}

STSPY_NOINLINE uint64_t stspy_native_level_4(uint64_t state) {
    uint64_t result = stspy_native_level_5(state + 4);
    stspy_deep_native_sink ^= result + state;
    return result ^ 4;
}

STSPY_NOINLINE uint64_t stspy_native_level_3(uint64_t state) {
    uint64_t result = stspy_native_level_4(state + 3);
    stspy_deep_native_sink ^= result + state;
    return result ^ 3;
}

STSPY_NOINLINE uint64_t stspy_native_level_2(uint64_t state) {
    uint64_t result = stspy_native_level_3(state + 2);
    stspy_deep_native_sink ^= result + state;
    return result ^ 2;
}

STSPY_NOINLINE uint64_t stspy_native_level_1(uint64_t state) {
    uint64_t result = stspy_native_level_2(state + 1);
    stspy_deep_native_sink ^= result + state;
    return result ^ 1;
}

unsigned char primitiveDeepNativeStackMetadata[2] = {1, 0xff};

void primitiveDeepNativeStack(void) {
    uint64_t state = stspy_deep_native_sink + 0x12345678ULL;

    for (int batch = 0; batch < 3; batch++) {
        state = stspy_native_level_1(state + (uint64_t)batch);
    }

    stspy_deep_native_sink = state;
}
