#define _GNU_SOURCE

#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/resource.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static long long monotonic_ns(void) {
    struct timespec now;
    if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) {
        perror("clock_gettime");
        exit(125);
    }
    return (long long)now.tv_sec * 1000000000LL + now.tv_nsec;
}

int main(int argc, char **argv) {
    if (argc < 3) {
        fprintf(stderr, "usage: %s METRICS_FILE COMMAND [ARG ...]\n", argv[0]);
        return 125;
    }

    long long started = monotonic_ns();
    pid_t child = fork();
    if (child < 0) {
        perror("fork");
        return 125;
    }
    if (child == 0) {
        execvp(argv[2], &argv[2]);
        perror("execvp");
        _exit(127);
    }

    int status = 0;
    struct rusage usage;
    while (wait4(child, &status, 0, &usage) < 0) {
        if (errno == EINTR) {
            continue;
        }
        perror("wait4");
        return 125;
    }
    long long wall_ms = (monotonic_ns() - started + 500000LL) / 1000000LL;
    int exit_code = WIFEXITED(status) ? WEXITSTATUS(status) : 128 + WTERMSIG(status);

    FILE *metrics = fopen(argv[1], "w");
    if (metrics == NULL) {
        perror("fopen metrics");
        return 125;
    }
    fprintf(metrics, "%lld\t%ld\t%d\n", wall_ms, usage.ru_maxrss, exit_code);
    if (fclose(metrics) != 0) {
        perror("fclose metrics");
        return 125;
    }
    return exit_code;
}
