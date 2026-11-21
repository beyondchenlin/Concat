package app.concat.jniregression;

/** No Android services, network, user files, model calls, or native callbacks. */
public class ProbeFixture {
    public boolean instanceValue = false;
    public static boolean staticValue = false;

    public ProbeFixture() {}

    public boolean echo(boolean value) {
        return value;
    }

    public static boolean staticEcho(boolean value) {
        return value;
    }

    public static void throwExpected() {
        throw new IllegalStateException("BKCAT_JNI_PROBE_EXPECTED");
    }

    /** The override distinguishes virtual from nonvirtual dispatch. */
    public static final class Child extends ProbeFixture {
        public Child() {}

        @Override
        public boolean echo(boolean value) {
            return !value;
        }
    }
}

