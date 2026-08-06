/*
 * suanctl 内置 NCCL all_reduce 基准（最小实现，供 suanctl nccl 子命令调用）。
 * 编译条件：系统存在 nccl.h 与 libnccl（构建时探测）；本文件不包含 NCCL 时
 * 由 build.rs 跳过，运行时报告不可用。
 *
 * 行为：在指定的 N 个 GPU 上执行 all_reduce（sum, float），按 NCCL 通用公式
 * 2 * (n-1) / n 折算有效带宽，输出结构化文本。
 * 参数：-n <gpus>（缺省=全部） -b <bytes>（缺省 256MiB） -i <iters>（缺省 50）
 */
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <time.h>

#include <cuda_runtime.h>
#include <nccl.h>

#define NCCL_CHECK(call)                                                    \
    do {                                                                    \
        ncclResult_t status = (call);                                       \
        if (status != ncclSuccess) {                                        \
            fprintf(stderr, "NCCL 错误 %s:%d: %s\n", __FILE__, __LINE__,    \
                    ncclGetErrorString(status));                            \
            return 1;                                                       \
        }                                                                   \
    } while (0)

#define CUDA_CHECK(call)                                                    \
    do {                                                                    \
        cudaError_t status = (call);                                        \
        if (status != cudaSuccess) {                                        \
            fprintf(stderr, "CUDA 错误 %s:%d: %s\n", __FILE__, __LINE__,    \
                    cudaGetErrorString(status));                            \
            return 1;                                                       \
        }                                                                   \
    } while (0)

static double now_seconds() {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (double)ts.tv_sec + (double)ts.tv_nsec / 1e9;
}

extern "C" int run_nccl_allreduce_test(int argc, char **argv) {
    int requested = 0;
    long long bytes = 1LL << 28;  // 256 MiB
    int iterations = 50;

    for (int i = 1; i < argc - 1; i += 2) {
        std::string key = argv[i];
        if (key == "-n") {
            requested = atoi(argv[i + 1]);
        } else if (key == "-b") {
            bytes = atoll(argv[i + 1]);
        } else if (key == "-i") {
            iterations = atoi(argv[i + 1]);
        }
    }

    int device_count = 0;
    CUDA_CHECK(cudaGetDeviceCount(&device_count));
    if (device_count < 2) {
        fprintf(stdout, "NCCL all_reduce 需要至少 2 张 GPU（当前 %d 张）\n", device_count);
        return 0;
    }
    int nranks = requested > 0 ? (requested < device_count ? requested : device_count)
                               : device_count;

    ncclComm_t comms[32];
    cudaStream_t streams[32];
    int rank_ids[32];
    void *buffers[32];
    for (int rank = 0; rank < nranks; ++rank) {
        rank_ids[rank] = rank;
        CUDA_CHECK(cudaSetDevice(rank));
        CUDA_CHECK(cudaStreamCreate(&streams[rank]));
        CUDA_CHECK(cudaMalloc(&buffers[rank], (size_t)bytes));
        CUDA_CHECK(cudaMemset(buffers[rank], 1, (size_t)bytes));
    }
    NCCL_CHECK(ncclCommInitAll(comms, nranks, rank_ids));

    // warmup
    for (int rank = 0; rank < nranks; ++rank) {
        NCCL_CHECK(ncclAllReduce(buffers[rank], buffers[rank], (size_t)bytes / sizeof(float),
                                 ncclFloat, ncclSum, comms[rank], streams[rank]));
    }
    for (int rank = 0; rank < nranks; ++rank) {
        CUDA_CHECK(cudaStreamSynchronize(streams[rank]));
    }

    double start = now_seconds();
    for (int iter = 0; iter < iterations; ++iter) {
        for (int rank = 0; rank < nranks; ++rank) {
            NCCL_CHECK(ncclAllReduce(buffers[rank], buffers[rank], (size_t)bytes / sizeof(float),
                                     ncclFloat, ncclSum, comms[rank], streams[rank]));
        }
        for (int rank = 0; rank < nranks; ++rank) {
            CUDA_CHECK(cudaStreamSynchronize(streams[rank]));
        }
    }
    double elapsed = now_seconds() - start;

    // NCCL all_reduce 有效带宽：2 * (n-1) / n 折算
    double factor = 2.0 * (nranks - 1) / (double)nranks;
    double bandwidth_gb_s = (double)bytes * factor * iterations / elapsed / 1e9;

    fprintf(stdout,
            "NCCL all_reduce 基准：%d GPU · 数据量 %.2f MiB · %d 次迭代 · %.3f s\n",
            nranks, (double)bytes / 1048576.0, iterations, elapsed);
    fprintf(stdout, "有效带宽：%.2f GB/s（sum 归约，float）\n", bandwidth_gb_s);

    for (int rank = 0; rank < nranks; ++rank) {
        NCCL_CHECK(ncclCommDestroy(comms[rank]));
        CUDA_CHECK(cudaStreamDestroy(streams[rank]));
        CUDA_CHECK(cudaFree(buffers[rank]));
    }
    return 0;
}
