/* xg — print "x y w h" (root-relative geometry) of an X window.
 * Used by verify-mode screenshots: Tauri's own outer_position() can report
 * a phantom companion window, so the on-screen position comes from X itself.
 * Usage: xg <window-id-hex>
 */
#include <X11/Xlib.h>
#include <stdio.h>
#include <stdlib.h>

int main(int argc, char **argv) {
    if (argc < 2) return 2;
    Window w = (Window)strtoul(argv[1], 0, 16);
    Display *d = XOpenDisplay(NULL);
    if (!d) {
        fprintf(stderr, "xg: cannot open display\n");
        return 1;
    }
    int x = 0, y = 0;
    Window child = 0;
    XTranslateCoordinates(d, w, DefaultRootWindow(d), 0, 0, &x, &y, &child);
    int gx = 0, gy = 0;
    unsigned gw = 0, gh = 0, gb = 0, gd = 0;
    Window root = 0;
    XGetGeometry(d, w, &root, &gx, &gy, &gw, &gh, &gb, &gd);
    XWindowAttributes a;
    XGetWindowAttributes(d, w, &a);
    if (a.map_state != IsViewable) {
        fprintf(stderr, "xg: window not viewable\n");
        XCloseDisplay(d);
        return 1;
    }
    printf("%d %d %u %u\n", x, y, gw, gh);
    XCloseDisplay(d);
    return 0;
}
