// Bulk driver for upstream macroxue/bridge-solver (built by release-compare.sh).
// Upstream's CLI solves one deal per process and its caches are globals that
// re-grow each time, so timing it per deal penalises it; this solves every
// deal of a file in one process through its Solve(). Input: lines of
// "N:S.H.D.C ..." (fixtures/divergence/lockstep-200.pbn). Output: per deal,
// its index then North-South tricks for strains NT,S,H,D,C x leaders W,E,N,S.
#include <cstdio>
#include <cstring>
#include <string>
#include <vector>
#include "solver.h"
static bool Parse(const char* pbn, Deal* d) {
  if (strncmp(pbn, "N:", 2) != 0) return false;
  const int order[4] = {NORTH, EAST, SOUTH, WEST};
  const char* p = pbn + 2;
  for (int h = 0; h < 4; ++h) {
    while (*p == ' ') ++p;
    std::string text;
    for (; *p && *p != ' '; ++p) {
      if (*p == '.') text += ' ';
      else if (*p != '-') text += *p;
    }
    // Rewrite empty suits as '-', the void Solve's input form uses.
    std::string out, suit;
    int n = 0;
    for (size_t i = 0; i <= text.size(); ++i) {
      if (i == text.size() || text[i] == ' ') {
        out += (n++ ? " " : "") + (suit.empty() ? std::string("-") : suit);
        suit.clear();
      } else {
        suit += text[i];
      }
    }
    if (n != 4) return false;
    d->hands[order[h]] = out;
  }
  return true;
}
int main(int argc, char** argv) {
  if (argc != 2) { fprintf(stderr, "usage: driver FILE\n"); return 2; }
  FILE* f = fopen(argv[1], "r");
  if (!f) { perror(argv[1]); return 2; }
  std::vector<Deal> deals;
  char line[256];
  while (fgets(line, sizeof line, f)) {
    line[strcspn(line, "\r\n")] = '\0';
    if (line[0] != 'N') continue;
    Deal d;
    if (!Parse(line, &d)) { fprintf(stderr, "bad deal: %s\n", line); return 1; }
    deals.push_back(ParseDeal(d));
  }
  const std::vector<int> trumps = {NOTRUMP, SPADE, HEART, DIAMOND, CLUB};
  const std::vector<int> leads = {WEST, EAST, NORTH, SOUTH};
  for (size_t i = 0; i < deals.size(); ++i) {
    std::string out;
    Solve(deals[i], trumps, leads, [](int) {},
          [&out](int, int, int ns) { out += " " + std::to_string(ns); }, [](int) {});
    printf("%zu%s\n", i + 1, out.c_str());
  }
}
