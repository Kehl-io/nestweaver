import java.util.List;
import java.util.ArrayList;

public interface Greeter {
    String greet(String name);
    String farewell(String name);
}

public class SimpleGreeter implements Greeter {
    private String prefix;

    public SimpleGreeter(String prefix) {
        this.prefix = prefix;
    }

    @Override
    public String greet(String name) {
        return prefix + " Hello, " + name + "!";
    }

    @Override
    public String farewell(String name) {
        return prefix + " Goodbye, " + name + "!";
    }

    public static void main(String[] args) {
        SimpleGreeter greeter = new SimpleGreeter(">>>");
        String result = greeter.greet("world");
        System.out.println(result);
    }
}
