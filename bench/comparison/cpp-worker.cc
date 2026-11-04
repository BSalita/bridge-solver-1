// A batch worker for the C++ reference (macroxue/bridge-solver), so that
// `solver-bench engines --cpp-worker` can time it against this port, DDS and
// ddss on the same deals.
//
// The reference keeps its caches in globals and is not thread-safe, so its
// way onto many cores is many processes. solver-bench starts one of these per
// thread it is allowed and hands each work items over a pipe, hardest first,
// as it does its own threads. A worker lives for the whole benchmark, so the
// reference keeps its grown caches between deals, which is how it runs best
// (one process per deal costs it about 2%).
//
// Protocol, one line each way per work item:
//   in:   <id> <strain> <PBN deal>     strain: S H D C N
//   out:  <id> <N> <E> <S> <W>          tricks for each seat as declarer
// The PBN deal is "N:AKQ.JT9.876.5432 ..." with any first seat, hands
// clockwise from it. A malformed line gets "<id> error" and the worker goes on.
//
// Build: bench/comparison/build-cpp-worker.sh <reference checkout> [OUT].
// It compiles this file with the reference's solver.cc and the reference's
// own PGO recipe.
#include <stdio.h>
#include <string.h>

#include <string>
#include <vector>

#include "solver.h"

namespace {

int SeatFromPbn(char c) {
  switch (c) {
    case 'N': case 'n': return NORTH;
    case 'E': case 'e': return EAST;
    case 'S': case 's': return SOUTH;
    case 'W': case 'w': return WEST;
  }
  return -1;
}

int StrainFromChar(char c) {
  switch (c) {
    case 'S': case 's': return SPADE;
    case 'H': case 'h': return HEART;
    case 'D': case 'd': return DIAMOND;
    case 'C': case 'c': return CLUB;
    case 'N': case 'n': return NOTRUMP;
  }
  return -1;
}

// Seats clockwise: West, North, East, South is the reference's enum order.
int NextSeat(int seat) { return (seat + 1) % NUM_SEATS; }

// "N:AKQ.JT9.876.5432 ..." to the reference's "AKQ JT9 876 5432" per seat,
// '-' for a void. False when the deal is not four hands of four suits.
bool ParsePbn(const char* pbn, Deal* deal) {
  while (*pbn == ' ') ++pbn;
  int seat = SeatFromPbn(pbn[0]);
  if (seat < 0 || pbn[1] != ':') return false;
  const char* p = pbn + 2;
  for (int h = 0; h < NUM_SEATS; ++h) {
    while (*p == ' ') ++p;
    std::string hand, suit;
    int suits = 0;
    for (;; ++p) {
      if (*p == '.' || *p == ' ' || *p == '\0' || *p == '\n' || *p == '\r') {
        hand += suit.empty() ? std::string("-") : suit;
        suit.clear();
        ++suits;
        if (*p != '.') break;
        hand += ' ';
      } else {
        suit += *p;
      }
    }
    if (suits != NUM_SUITS) return false;
    deal->hands[seat] = hand;
    seat = NextSeat(seat);
  }
  return true;
}

}  // namespace

int main() {
  // Each answer is flushed as soon as it is written: solver-bench waits for
  // it before handing this worker the next item.
  setvbuf(stdout, nullptr, _IOFBF, 1 << 16);
  char line[512];
  while (fgets(line, sizeof line, stdin)) {
    long id;
    char strain_char;
    int consumed = 0;
    if (sscanf(line, "%ld %c %n", &id, &strain_char, &consumed) != 2) continue;
    int trump = StrainFromChar(strain_char);
    Deal deal;
    if (trump < 0 || !ParsePbn(line + consumed, &deal)) {
      printf("%ld error\n", id);
      fflush(stdout);
      continue;
    }
    // Exits on a deal that is not 52 distinct cards; solver-bench reports
    // a worker that dies.
    deal = ParseDeal(deal);
    int num_tricks = NumTricks(deal);
    int declarer_tricks[NUM_SEATS] = {};
    // Lead seats in the CLI's order: each search's guess starts from the
    // previous answer, so the order is part of what gets searched.
    Solve(
        deal, {trump}, {WEST, EAST, NORTH, SOUTH}, [](int) {},
        [&](int, int lead_seat, int ns_tricks) {
          // The opening lead is made by declarer's left-hand opponent.
          int declarer = (lead_seat + NUM_SEATS - 1) % NUM_SEATS;
          declarer_tricks[declarer] = IsNs(declarer) ? ns_tricks : num_tricks - ns_tricks;
        },
        [](int) {});
    printf("%ld %d %d %d %d\n", id, declarer_tricks[NORTH], declarer_tricks[EAST],
           declarer_tricks[SOUTH], declarer_tricks[WEST]);
    fflush(stdout);
  }
  return 0;
}
