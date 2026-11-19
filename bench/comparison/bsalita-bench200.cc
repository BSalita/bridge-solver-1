// Driver for Bob Salita's solver library (BSalita/bridge-solver b12bb3f), built
// by build-bsalita.sh. His command line solves one deal per process; this
// solves every deal in a file of PBN deals ("N:..." one per line) as a full
// table in one process, single-threaded, through the library's public Solve():
// the call his solver-cli.cc makes, with the same strain and lead-seat order.
// One process matters: see "The reference was being timed one process per
// deal" in bench/results/release-profile.md.
// Prints one line per deal: 20 trick counts, strains N S H D C, leads W E N S.
#include <stdio.h>
#include <string.h>

#include <string>
#include <vector>

#include "solver.h"

int main(int argc, char* argv[]) {
  if (argc != 2) {
    fprintf(stderr, "usage: %s deals.pbn\n", argv[0]);
    return 2;
  }
  FILE* f = fopen(argv[1], "r");
  if (!f) {
    perror(argv[1]);
    return 2;
  }
  std::vector<Deal> deals;
  char line[512];
  while (fgets(line, sizeof line, f)) {
    std::string s = line;
    while (!s.empty() && (s.back() == '\n' || s.back() == '\r')) s.pop_back();
    if (s.empty()) continue;
    // PbnToDeal wants a void left empty; the corpus writes it '-'.
    std::string t;
    for (char c : s)
      if (c != '-') t += c;
    s = t;
    Deal d;
    if (!PbnToDeal(s.c_str(), &d)) {
      fprintf(stderr, "bad deal: %s\n", s.c_str());
      return 1;
    }
    deals.push_back(d);
  }
  fclose(f);

  std::vector<int> trumps = {NOTRUMP, SPADE, HEART, DIAMOND, CLUB};
  std::vector<int> lead_seats = {WEST, EAST, NORTH, SOUTH};
  for (const Deal& deal : deals) {
    int num_tricks = NumTricks(deal);
    std::string out;
    auto none = [](int) {};
    auto seat_done = [&out, num_tricks](int, int lead_seat, int ns_tricks) {
      char buf[8];
      snprintf(buf, sizeof buf, " %d", IsNs(lead_seat) ? num_tricks - ns_tricks : ns_tricks);
      out += buf;
    };
    Solve(deal, trumps, lead_seats, none, seat_done, none);
    printf("%s\n", out.c_str() + 1);
  }
  return 0;
}
