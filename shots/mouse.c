/* mouse — XTEST pointer helper for verifying window drag/resize (no xdotool
 * on this host). Moves the REAL pointer; use save/restore around a session.
 *
 * Usage:
 *   mouse save                    save pointer position to /tmp/.mousepos
 *   mouse restore                 restore the saved pointer position
 *   mouse drag <x> <y> <dx> <dy>  move to (x,y), press button 1, step to
 *                                 (x+dx,y+dy) over ~300 ms, release
 */
#include <X11/Xlib.h>
#include <X11/extensions/XTest.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static const char *SAVE_FILE = "/tmp/.mousepos";

static void move_to(Display *d, int x, int y) {
    XTestFakeMotionEvent(d, -1, x, y, 0);
    XFlush(d);
}

int main(int argc, char **argv) {
    if (argc < 2) return 2;
    Display *d = XOpenDisplay(NULL);
    if (!d) {
        fprintf(stderr, "mouse: cannot open display\n");
        return 1;
    }
    Window root = DefaultRootWindow(d);

    if (strcmp(argv[1], "save") == 0) {
        int x = 0, y = 0, cx = 0, cy = 0;
        unsigned int mask = 0;
        Window child = 0;
        XQueryPointer(d, root, &root, &child, &cx, &cy, &x, &y, &mask);
        FILE *f = fopen(SAVE_FILE, "w");
        if (f) {
            fprintf(f, "%d %d\n", x, y);
            fclose(f);
        }
        printf("%d %d\n", x, y);
    } else if (strcmp(argv[1], "restore") == 0) {
        int x = 0, y = 0;
        FILE *f = fopen(SAVE_FILE, "r");
        if (!f || fscanf(f, "%d %d", &x, &y) != 2) {
            fprintf(stderr, "mouse: no saved position\n");
            return 1;
        }
        fclose(f);
        move_to(d, x, y);
        printf("restored %d %d\n", x, y);
    } else if (strcmp(argv[1], "drag") == 0) {
        if (argc < 6) return 2;
        int x = atoi(argv[2]), y = atoi(argv[3]);
        int dx = atoi(argv[4]), dy = atoi(argv[5]);
        const int steps = 12;
        move_to(d, x, y);
        usleep(80 * 1000);
        XTestFakeButtonEvent(d, 1, True, -1);
        XFlush(d);
        usleep(120 * 1000);
        for (int i = 1; i <= steps; i++) {
            move_to(d, x + dx * i / steps, y + dy * i / steps);
            usleep(20 * 1000);
        }
        usleep(60 * 1000);
        XTestFakeButtonEvent(d, 1, False, -1);
        XFlush(d);
        usleep(80 * 1000);
        printf("dragged from %d,%d by %d,%d\n", x, y, dx, dy);
    } else {
        return 2;
    }
    XCloseDisplay(d);
    return 0;
}
