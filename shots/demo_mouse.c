/* demo_mouse — XTEST pointer/keyboard driver for scripted UI demos.
 *
 * Moves the REAL pointer and sends real events. Every button press is
 * paired with a release before the process exits.
 *
 * usage:
 *   demo_mouse save                 print current pointer "x y"
 *   demo_mouse restore <x> <y>      move pointer back
 *   demo_mouse move <x> <y>
 *   demo_mouse click <x> <y>        left button
 *   demo_mouse rclick <x> <y>       right button (context menu)
 *   demo_mouse type <text>          ASCII (letters/digits/-_. )
 *   demo_mouse winmove <winid> <x> <y>   move window (hex id, root coords)
 *
 * Note: exits via exit(0) instead of XCloseDisplay — on this host's X
 * server, a client that sent XTestFakeButtonEvent hangs on close. All
 * events are XFlush()ed before exit, so nothing is lost.
 *
 * All XTEST events use device id 0: on the TigerVNC server (and some
 * hosts) the default pointer (-1) hangs the client on button events,
 * while device 0 delivers them cleanly.
 */
#include <X11/Xlib.h>
#include <X11/extensions/XTest.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define DEV 0

static void move_to(Display *d, int x, int y) {
    XTestFakeMotionEvent(d, DEV, x, y, 0);
    XFlush(d);
}

static void press(Display *d, unsigned int button) {
    XTestFakeButtonEvent(d, button, True, DEV);
    XFlush(d);
    usleep(100 * 1000);
    XTestFakeButtonEvent(d, button, False, DEV);
    XFlush(d);
    usleep(80 * 1000);
}

int main(int argc, char **argv) {
    if (argc < 2) return 2;
    Display *d = XOpenDisplay(NULL);
    if (!d) return 1;
    Window root = DefaultRootWindow(d);

    if (strcmp(argv[1], "save") == 0) {
        int x = 0, y = 0, cx = 0, cy = 0;
        unsigned int mask = 0;
        Window child = 0;
        XQueryPointer(d, root, &child, &child, &cx, &cy, &x, &y, &mask);
        printf("%d %d\n", x, y);
    } else if (strcmp(argv[1], "restore") == 0) {
        if (argc >= 4) move_to(d, atoi(argv[2]), atoi(argv[3]));
    } else if (strcmp(argv[1], "move") == 0) {
        move_to(d, atoi(argv[2]), atoi(argv[3]));
        usleep(50 * 1000);
    } else if (strcmp(argv[1], "click") == 0) {
        move_to(d, atoi(argv[2]), atoi(argv[3]));
        usleep(80 * 1000);
        press(d, 1);
    } else if (strcmp(argv[1], "rclick") == 0) {
        move_to(d, atoi(argv[2]), atoi(argv[3]));
        usleep(80 * 1000);
        press(d, 3);
    } else if (strcmp(argv[1], "type") == 0) {
        for (const char *s = argv[2]; *s; s++) {
            KeyCode kc = XKeysymToKeycode(d, XStringToKeysym(s));
            if (!kc) continue;
            XTestFakeKeyEvent(d, kc, True, DEV);
            XFlush(d);
            usleep(25 * 1000);
            XTestFakeKeyEvent(d, kc, False, DEV);
            XFlush(d);
            usleep(40 * 1000);
        }
    } else if (strcmp(argv[1], "winmove") == 0) {
        Window w = (Window)strtoul(argv[2], NULL, 16);
        XMoveWindow(d, w, atoi(argv[3]), atoi(argv[4]));
        XFlush(d);
        usleep(100 * 1000);
    } else {
        return 2;
    }
    XFlush(d);
    exit(0);
}
