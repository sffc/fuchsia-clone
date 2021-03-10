// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <sys/select.h>

#include <fbl/unique_fd.h>
#include <zxtest/zxtest.h>

namespace {

// Like with poll and ppoll, Fuchsia guarantees that selecting on 0
// fds in the fd_sets is equivalent to sleeping until the timeout.
//
// This is extremely similar to the tests of poll and ppoll in
// fdio_poll.cc.

TEST(Select, SelectZeroFds) {
  fd_set readfds;
  fd_set writefds;
  fd_set exceptfds;
  FD_ZERO(&readfds);
  FD_ZERO(&writefds);
  FD_ZERO(&exceptfds);

  constexpr std::chrono::duration minimum_duration = std::chrono::milliseconds(1);

  struct timeval timeout = {
      .tv_usec = std::chrono::microseconds(minimum_duration).count(),
  };
  const auto begin = std::chrono::steady_clock::now();
  EXPECT_EQ(select(0, &readfds, &writefds, &exceptfds, &timeout), 0, "%s", strerror(errno));
  EXPECT_GE(std::chrono::steady_clock::now() - begin, minimum_duration);

  // All bits in all the fd sets should be 0.
  for (int fd = 0; fd < FD_SETSIZE; ++fd) {
    EXPECT_FALSE(FD_ISSET(fd, &readfds));
    EXPECT_FALSE(FD_ISSET(fd, &writefds));
    EXPECT_FALSE(FD_ISSET(fd, &exceptfds));
  }
}

TEST(Select, Pipe) {
  std::array<fbl::unique_fd, 2> fds;
  int int_fds[fds.size()];
  ASSERT_EQ(pipe(int_fds), 0, "%s", strerror(errno));
  fds[0].reset(int_fds[0]);
  fds[1].reset(int_fds[1]);

  constexpr std::chrono::duration minimum_duration = std::chrono::milliseconds(1);

  struct timeval timeout = {
      .tv_usec = std::chrono::microseconds(minimum_duration).count(),
  };

  {
    fd_set readfds;
    FD_ZERO(&readfds);
    FD_SET(fds[0].get(), &readfds);

    EXPECT_EQ(select(fds[0].get() + 1, &readfds, nullptr, nullptr, &timeout), 0, "%s",
              strerror(errno));
    EXPECT_FALSE(FD_ISSET(fds[0].get(), &readfds));
  }

  {
    char c;
    ASSERT_EQ(write(fds[1].get(), &c, sizeof(c)), sizeof(c), "%s", strerror(errno));

    fd_set readfds;
    FD_ZERO(&readfds);
    FD_SET(fds[0].get(), &readfds);

    EXPECT_EQ(select(fds[0].get() + 1, &readfds, nullptr, nullptr, &timeout), 1, "%s",
              strerror(errno));
    EXPECT_TRUE(FD_ISSET(fds[0].get(), &readfds));
  }

  {
    ASSERT_EQ(close(fds[1].get()), 0, "%s", strerror(errno));

    fd_set readfds;
    FD_ZERO(&readfds);
    FD_SET(fds[0].get(), &readfds);

    EXPECT_EQ(select(fds[0].get() + 1, &readfds, nullptr, nullptr, &timeout), 1, "%s",
              strerror(errno));
    EXPECT_TRUE(FD_ISSET(fds[0].get(), &readfds));
  }
}

TEST(Select, SelectNegative) {
  fd_set readfds;
  fd_set writefds;
  fd_set exceptfds;
  FD_ZERO(&readfds);
  FD_ZERO(&writefds);
  FD_ZERO(&exceptfds);

  {
    struct timeval timeout = {
        .tv_sec = -1,
    };
    EXPECT_EQ(select(0, &readfds, &writefds, &exceptfds, &timeout), -1);
    EXPECT_EQ(errno, EINVAL, "%s", strerror(errno));
  }
  {
    struct timeval timeout = {
        .tv_usec = -1,
    };
    EXPECT_EQ(select(0, &readfds, &writefds, &exceptfds, &timeout), -1);
    EXPECT_EQ(errno, EINVAL, "%s", strerror(errno));
  }
}

}  // namespace
