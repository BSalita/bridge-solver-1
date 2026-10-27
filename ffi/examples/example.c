/*
 * Calling bridge-solver from C. Also the C interface's smoke test: CI builds
 * it against the header and the library and runs it, and it exits non-zero
 * if any answer is wrong.
 *
 *   cc ffi/examples/example.c -Iffi/include -Lffi/target/release -lbridgesolver
 */

#include <stdio.h>
#include <string.h>

#include "bridge_solver.h"

static const char *DEAL =
    "N:AKQT3.J6.KJ42.95 652.AK42.AQ87.T4 J74.QT95.T.AK863 98.873.9653.QJ72";

static int failures = 0;

static void expect(int ok, const char *what) {
    printf("%s %s\n", ok ? "ok  " : "FAIL", what);
    if (!ok) failures++;
}

int main(void) {
    printf("bridge-solver %s\n", bs_version());

    /* The table as numbers. */
    unsigned char tricks[20];
    expect(bs_dd_table_tricks(DEAL, tricks) == 0, "bs_dd_table_tricks succeeds");
    printf("     North in spades takes %d tricks\n", tricks[0 * 5 + 3]);
    expect(tricks[0 * 5 + 3] == 10, "North makes 10 tricks in spades");

    /* The same as JSON. */
    char *table = bs_dd_table(DEAL);
    printf("     %s\n", table);
    expect(strstr(table, "[8,7,8,10,9]") != NULL, "bs_dd_table returns the same table");
    bs_free_string(table);

    char *par = bs_par(DEAL, "None");
    printf("     %s\n", par);
    expect(strstr(par, "\"score_ns\"") != NULL, "bs_par answers");
    bs_free_string(par);

    /* East has cashed two hearts; can East claim one more trick? */
    char *claim = bs_check_claim(
        "{\"dealstr\":\"N:AKQT3.J6.KJ42.95 652.AK42.AQ87.T4 J74.QT95.T.AK863 98.873.9653.QJ72\","
        "\"trump\":\"S\",\"declarer\":\"N\",\"leader\":\"E\","
        "\"plays\":[\"HA\",\"H5\",\"H3\",\"H6\",\"HK\",\"H9\",\"H7\",\"HJ\"],"
        "\"claimer\":\"E\",\"total_tricks\":3}");
    printf("     %s\n", claim);
    expect(strstr(claim, "\"verdict\":\"invalid\"") != NULL,
           "a third trick is not a claim: declarer may ruff the DA");
    bs_free_string(claim);

    BsAnalyzer *analyzer = bs_analyzer_new();
    char *trace = bs_analyzer_play(analyzer,
        "{\"dealstr\":\"N:AKQT3.J6.KJ42.95 652.AK42.AQ87.T4 J74.QT95.T.AK863 98.873.9653.QJ72\","
        "\"trump\":\"S\",\"declarer\":\"N\",\"leader\":\"E\",\"plays\":[\"HA\",\"H5\"]}");
    expect(strstr(trace, "\"contract_tricks\":10") != NULL, "bs_analyzer_play traces the play");
    bs_free_string(trace);
    bs_analyzer_free(analyzer);

    char *error = bs_dd_table("not a deal");
    printf("     %s\n", error);
    expect(strncmp(error, "{\"error\":", 9) == 0, "a bad deal is an error, not a crash");
    bs_free_string(error);

    expect(bs_solve(DEAL, "S", "E") == 10, "bs_solve: North-South take 10 in spades");

    printf("%s\n", failures ? "FAILED" : "all passed");
    return failures ? 1 : 0;
}
